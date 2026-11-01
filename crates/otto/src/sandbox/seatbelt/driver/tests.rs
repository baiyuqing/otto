use tokio_util::sync::CancellationToken;

use super::{Options, SeatbeltDriver};
use crate::sandbox::{Error, NetworkMode, UnavailableReason};

/// The shape a session has when it starts in `$HOME`, which contains
/// `$HOME/Library/Caches`.
#[tokio::test]
async fn workspace_containing_the_cache_base_is_policy_unsupported() {
    let base = tempfile::TempDir::new().expect("temp base");
    let cache = base.path().join("Caches");
    std::fs::create_dir_all(&cache).expect("cache directory");
    let workspace = base.path().to_str().expect("workspace text").to_string();
    let options = Options {
        workspace: workspace.clone(),
        shell: "/bin/zsh".to_string(),
        home: workspace,
        cache_base: cache.to_str().expect("cache text").to_string(),
        network: Some(NetworkMode::Deny),
        ..Options::default()
    };

    let error = SeatbeltDriver::open(options, &CancellationToken::new())
        .await
        .expect_err("a workspace containing the cache base is refused");

    assert_eq!(
        error,
        Error::unavailable(UnavailableReason::PolicyUnsupported)
    );
}

/// Real `sandbox-exec` fixtures for [`SeatbeltDriver::reconfigure`]: the
/// workspace, the host home and the cache base are separate 0700 canonical
/// directories, and `outside` holds a file the default profile denies.
#[cfg(target_os = "macos")]
struct Reconfigure {
    _root: tempfile::TempDir,
    workspace: std::path::PathBuf,
    outside: std::path::PathBuf,
    driver: SeatbeltDriver,
}

#[cfg(target_os = "macos")]
async fn reconfigure_fixture() -> Option<Reconfigure> {
    fixture_with(false).await
}

/// `git` makes the workspace a repository with one commit before the
/// driver opens, using the host's git outside any sandbox.
#[cfg(target_os = "macos")]
async fn fixture_with(git: bool) -> Option<Reconfigure> {
    use std::os::unix::fs::PermissionsExt as _;

    if let Some(reason) = super::unavailable_reason() {
        eprintln!("skipping: {reason}");
        return None;
    }
    let root = tempfile::TempDir::new().expect("temp root");
    let base = std::fs::canonicalize(root.path()).expect("canonical root");
    let directory = |name: &str| {
        let path = base.join(name);
        std::fs::create_dir_all(&path).expect("directory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("directory mode");
        path
    };
    let workspace = directory("workspace");
    if git {
        for args in [
            &["init", "-q"][..],
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            let status = std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(&workspace)
                .status()
                .expect("run host git");
            assert!(status.success(), "host git {args:?} failed");
        }
    }
    let outside = directory("outside");
    std::fs::write(outside.join("note"), "outside-contents").expect("outside file");
    let driver = SeatbeltDriver::open(
        Options {
            workspace: workspace.to_str().expect("workspace").to_string(),
            workspaces: vec![workspace.to_str().expect("workspace").to_string()],
            shell: "/bin/sh".to_string(),
            home: directory("host-home").to_str().expect("home").to_string(),
            cache_base: directory("user-cache").to_str().expect("cache").to_string(),
            read_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            network: Some(NetworkMode::Deny),
        },
        &CancellationToken::new(),
    )
    .await
    .expect("seatbelt open");
    Some(Reconfigure {
        _root: root,
        workspace,
        outside,
        driver,
    })
}

/// Runs `script` under the driver and returns whether it exited 0 and its
/// stdout.
#[cfg(target_os = "macos")]
async fn run_script(fixture: &Reconfigure, script: &str) -> (bool, String) {
    use crate::sandbox::{Driver as _, Request, Streams};

    let directories = fixture.driver.private_directories();
    let mut stdout = Vec::new();
    let (status, result) = fixture
        .driver
        .execute(
            Request {
                stdin: None,
                argv: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
                dir: fixture.workspace.clone(),
                env: vec![
                    "PATH=/usr/bin:/bin".to_string(),
                    format!("HOME={}", directories.home.display()),
                ],
                ..Default::default()
            },
            Streams {
                stdout: &mut stdout,
                stderr: &mut Vec::new(),
            },
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_ok(), "{result:?}");
    (
        status.code == 0 && !status.signaled,
        String::from_utf8_lossy(&stdout).into_owned(),
    )
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn reconfigure_applies_new_read_paths_and_network_in_the_same_private_tree() {
    use crate::sandbox::Driver as _;

    let Some(fixture) = reconfigure_fixture().await else {
        return;
    };
    let cat = format!("cat '{}'", fixture.outside.join("note").display());
    let (readable, _) = run_script(&fixture, &cat).await;
    assert!(!readable, "the profile must deny a file outside its roots");
    assert!(fixture.driver.capabilities().network_deny);
    let private = fixture.driver.private_directories();

    fixture
        .driver
        .reconfigure(
            vec![fixture.outside.to_str().expect("outside").to_string()],
            Vec::new(),
            vec![fixture.workspace.to_str().expect("workspace").to_string()],
            NetworkMode::Allow,
            &CancellationToken::new(),
        )
        .await
        .expect("reconfigure");

    assert_eq!(fixture.driver.private_directories(), private);
    let capabilities = fixture.driver.capabilities();
    assert!(capabilities.network_allow && !capabilities.network_deny);
    let (readable, output) = run_script(&fixture, &cat).await;
    assert!(readable, "the new read path must be readable");
    assert_eq!(output, "outside-contents");
    let (written, output) = run_script(
        &fixture,
        r#"printf kept > "$HOME/probe" && cat "$HOME/probe""#,
    )
    .await;
    assert!(written, "the private HOME must stay writable");
    assert_eq!(output, "kept");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn reconfigure_with_unchanged_settings_leaves_the_profile_file_alone() {
    let Some(fixture) = reconfigure_fixture().await else {
        return;
    };
    let cancel = CancellationToken::new();
    let read_paths = vec![fixture.outside.to_str().expect("outside").to_string()];
    fixture
        .driver
        .reconfigure(
            read_paths.clone(),
            Vec::new(),
            vec![fixture.workspace.to_str().expect("workspace").to_string()],
            NetworkMode::Allow,
            &cancel,
        )
        .await
        .expect("first");
    let written = std::fs::read(&fixture.driver.profile_path).expect("profile");

    fixture
        .driver
        .reconfigure(
            read_paths,
            Vec::new(),
            vec![fixture.workspace.to_str().expect("workspace").to_string()],
            NetworkMode::Allow,
            &cancel,
        )
        .await
        .expect("second");

    assert_eq!(
        std::fs::read(&fixture.driver.profile_path).expect("profile"),
        written
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_missing_read_path_reconfigure_keeps_the_driver_running() {
    use crate::sandbox::Driver as _;

    let Some(fixture) = reconfigure_fixture().await else {
        return;
    };
    let before = std::fs::read(&fixture.driver.profile_path).expect("profile");

    fixture
        .driver
        .reconfigure(
            vec!["/otto-reconfigure-missing-directory".to_string()],
            Vec::new(),
            vec![fixture.workspace.to_str().expect("workspace").to_string()],
            NetworkMode::Allow,
            &CancellationToken::new(),
        )
        .await
        .expect("a missing read path is ignored");

    assert_ne!(
        std::fs::read(&fixture.driver.profile_path).expect("profile"),
        before
    );
    assert!(fixture.driver.capabilities().network_allow);
    let (ok, output) = run_script(&fixture, "printf ok").await;
    assert!(ok);
    assert_eq!(output, "ok");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn reconfigure_on_a_closed_driver_is_refused() {
    use crate::sandbox::Driver as _;

    let Some(fixture) = reconfigure_fixture().await else {
        return;
    };
    fixture.driver.close().expect("close");
    let error = fixture
        .driver
        .reconfigure(
            Vec::new(),
            Vec::new(),
            vec![fixture.workspace.to_str().expect("workspace").to_string()],
            NetworkMode::Deny,
            &CancellationToken::new(),
        )
        .await
        .expect_err("closed");
    assert_eq!(error, Error::Closed);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn workspace_git_metadata_is_read_only_but_git_workflows_work() {
    let Some(fixture) = fixture_with(true).await else {
        return;
    };
    crate::workspace::initialize(&fixture.workspace, "").expect("workspace identity");
    let marker = fixture.workspace.join(crate::workspace::MARKER);
    let marker_before = std::fs::read(&marker).expect("read workspace identity");
    let config = fixture.workspace.join(".git/config");
    let before = std::fs::read(&config).expect("read .git/config");
    for script in [
        "echo x > .git/hooks/pre-commit",
        "echo '[x]' >> .git/config",
        "git config user.email a@b",
        "touch .git/commondir",
        "mv .git g2",
        "ln .git/config hl",
        "echo bad > .otto/workspace.json",
        "mv .otto moved-state",
        "ln .otto/workspace.json identity-link",
    ] {
        let (ok, _) = run_script(&fixture, script).await;
        assert!(!ok, "the sandbox allowed: {script}");
    }
    assert_eq!(std::fs::read(&config).expect("re-read"), before);
    assert!(fixture.workspace.join(".git").is_dir(), ".git was moved");
    assert_eq!(
        std::fs::read(&marker).expect("re-read identity"),
        marker_before
    );
    for script in [
        "git -c user.email=t@t -c user.name=t commit -q --allow-empty -m t",
        "git checkout -q -b topic",
        "echo ok > regular-file",
        "echo ok > .otto/regular-file",
    ] {
        let (ok, _) = run_script(&fixture, script).await;
        assert!(ok, "the sandbox refused: {script}");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn the_system_volumes_alias_does_not_reopen_a_denied_path() {
    let Some(fixture) = reconfigure_fixture().await else {
        return;
    };
    let path = fixture.outside.join("note");
    let (readable, _) = run_script(&fixture, &format!("cat '{}'", path.display())).await;
    assert!(!readable, "control: the denied path must be unreadable");
    let alias = format!("cat '/System/Volumes/Data{}'", path.display());
    let (readable, output) = run_script(&fixture, &alias).await;
    assert!(!readable, "the alias read the file: {output:?}");
}

/// Runs `script` and returns the exit status it produced.
#[cfg(target_os = "macos")]
async fn run_status(
    fixture: &Reconfigure,
    script: &str,
    report_denials: bool,
) -> crate::sandbox::ExitStatus {
    use crate::sandbox::{Driver as _, Request, Streams};

    let directories = fixture.driver.private_directories();
    let (status, result) = fixture
        .driver
        .execute(
            Request {
                stdin: None,
                argv: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
                dir: fixture.workspace.clone(),
                env: vec![
                    "PATH=/usr/bin:/bin".to_string(),
                    format!("HOME={}", directories.home.display()),
                ],
                report_denials,
            },
            Streams {
                stdout: &mut Vec::new(),
                stderr: &mut Vec::new(),
            },
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_ok(), "{result:?}");
    status
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_failed_command_reports_the_denied_read_only_when_asked() {
    let Some(fixture) = reconfigure_fixture().await else {
        return;
    };
    if !fixture.driver.monitor.as_ref().is_some_and(|m| m.running()) {
        eprintln!("skipping: `log stream` could not start");
        return;
    }
    let path = fixture.outside.join("note");
    let cat = format!("cat '{}'", path.display());
    // `log stream` needs time to start, so early denials can be missed.
    let mut status = crate::sandbox::ExitStatus::default();
    for _ in 0..10 {
        status = run_status(&fixture, &cat, true).await;
        if !status.denials.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_ne!(status.code, 0);
    let target = path.to_str().expect("path");
    assert!(
        status
            .denials
            .iter()
            .any(|d| d.operation.starts_with("file-read") && d.target == target),
        "no read denial for {target}: {:?}",
        status.denials
    );
    let status = run_status(&fixture, &cat, false).await;
    assert_ne!(status.code, 0);
    assert!(status.denials.is_empty(), "{:?}", status.denials);
}
