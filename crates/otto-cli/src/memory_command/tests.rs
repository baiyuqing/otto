use super::*;
use otto::memory::RememberRequest;
use std::collections::HashMap;

/// Writes a config file enabling memory and returns its path.
pub(super) fn memory_config(directory: &Path) -> String {
    let path = directory.join("otto.toml");
    std::fs::write(&path, "[memory]\nenabled = true\n").expect("write config");
    path.to_string_lossy().into_owned()
}

pub(super) fn store_dir(home: &Path) -> std::path::PathBuf {
    home.join(".otto").join("memory")
}

/// The runtime that `memory_config` resolves to under `home`.
pub(super) fn runtime(home: &Path) -> MemoryRuntime {
    MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: store_dir(home).to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    }
}

fn lookup(home: &Path) -> HashMap<String, String> {
    HashMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())])
}

/// Runs `otto memory ...` and returns its exit code, stdout and stderr.
pub(super) fn memory(args: &[&str], home: &Path) -> (i32, String, String) {
    let arguments: Vec<String> = args.iter().map(|value| value.to_string()).collect();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(&arguments, &mut stdout, &mut stderr, &lookup(home));
    (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

#[test]
fn status_reports_pending_candidates_in_the_current_scopes() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let config = memory_config(home.path());
    let cwd = workspace.path().to_string_lossy().into_owned();
    let status = || memory(&["status", "--config", &config, "--cwd", &cwd], home.path()).1;
    assert!(status().contains("pending: 0\n"), "{}", status());

    let runtime = runtime(home.path());
    let (service, user, _) =
        otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new()).expect("open");
    let current = otto::runtime::wiring::workspace_memory_scope(
        &runtime,
        &workspace.path().to_string_lossy(),
    )
    .expect("scope");
    for (scope, key) in [(user, "a"), (current, "b")] {
        service
            .propose(&otto::memory::ProposeRequest {
                scope,
                kind: "preference".into(),
                key: key.into(),
                text: key.into(),
                reason: "said so".into(),
                confidence: 0.5,
                source: otto::memory::Provenance {
                    origin: Some(otto::memory::Origin::Model),
                    ..otto::memory::Provenance::default()
                },
                ..otto::memory::ProposeRequest::default()
            })
            .expect("propose");
    }
    service.close().expect("close");
    assert!(status().contains("pending: 2\n"), "{}", status());
}

#[test]
fn status_reports_standing_usage_and_over_budget_records() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let config = memory_config(home.path());
    let cwd = workspace.path().to_string_lossy().into_owned();
    let status = || memory(&["status", "--config", &config, "--cwd", &cwd], home.path()).1;
    let empty = status();
    assert!(
        empty.contains("standing: 0 records, 0/2000 tokens\n"),
        "{empty}"
    );

    otto::fixtures::seed_over_budget_standing(&store_dir(home.path()));
    let over = status();
    let line = over
        .lines()
        .find(|line| line.starts_with("standing:"))
        .expect("standing line");
    assert!(
        line.starts_with("standing: 3 records, ")
            && line.ends_with("/2000 tokens, 1 not injected (over budget)"),
        "{line}"
    );
}

#[test]
fn status_reports_configured_memory() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let config = memory_config(home.path());

    let (code, stdout, stderr) = memory(
        &[
            "status",
            "--config",
            &config,
            "--cwd",
            &workspace.path().to_string_lossy(),
        ],
        home.path(),
    );
    assert_eq!(code, 0, "stderr = {stderr}");
    let path = format!("path: {}\n", store_dir(home.path()).display());
    for want in ["enabled: true", "backend: files", &path, "usable: true"] {
        assert!(stdout.contains(want), "stdout = {stdout:?}, want {want:?}");
    }
}

#[test]
fn has_store_is_true_only_after_the_identity_file_exists() {
    let home = tempfile::tempdir().expect("home");
    let file = otto_core::config::File::default();
    let lookup = lookup(home.path());
    let home_text = home.path().to_string_lossy().into_owned();
    assert!(!has_store(&file, &lookup, &home_text));
    otto::memory::files::FileStore::open(
        &store_dir(home.path()),
        otto::memory::repository::MemoryOptions::default(),
    )
    .expect("create store");
    assert!(has_store(&file, &lookup, &home_text));
}

#[test]
fn status_lists_skipped_files_without_their_content() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let config = memory_config(home.path());
    let dir = store_dir(home.path());
    otto::memory::files::FileStore::open(&dir, otto::memory::repository::MemoryOptions::default())
        .expect("create store");
    std::fs::write(dir.join("records").join("bad.md"), "body-secret-text\n").expect("bad");

    let (code, stdout, stderr) = memory(
        &[
            "status",
            "--config",
            &config,
            "--cwd",
            &workspace.path().to_string_lossy(),
        ],
        home.path(),
    );
    assert_eq!(code, 0, "stderr = {stderr}");
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("skipped: "))
        .collect();
    assert_eq!(lines.len(), 1, "stdout = {stdout:?}");
    assert!(lines[0].contains("bad.md"), "{}", lines[0]);
    assert!(!stdout.contains("body-secret-text"), "stdout = {stdout:?}");
}

/// A captured token must not survive into the status output.
#[test]
fn status_redacts_captured_chatgpt_credentials() {
    let root = tempfile::tempdir().expect("root");
    let workspace = tempfile::tempdir().expect("workspace");
    // Not a credential: a fixture value placed in the home path, which the
    // status output prints as part of the store path, so an uncollected
    // token is visible in the assertion.
    let token = "captured-chatgpt-token-fixture";
    let home = root.path().join(token);
    let auth_path = otto::auth::path_for_home(&home);
    std::fs::create_dir_all(auth_path.parent().expect("auth directory"))
        .expect("create auth directory");
    std::fs::write(&auth_path, format!(r#"{{"access_token":"{token}"}}"#))
        .expect("write credentials");
    let config = memory_config(&home);

    let (code, stdout, stderr) = memory(
        &[
            "status",
            "--config",
            &config,
            "--cwd",
            &workspace.path().to_string_lossy(),
        ],
        &home,
    );

    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("path: "), "stdout = {stdout:?}");
    assert!(
        !stdout.contains(token),
        "stdout leaked the token: {stdout:?}"
    );
    assert!(stdout.contains("usable: true"), "stdout = {stdout:?}");
}

#[test]
fn list_pages_all_scopes_only_when_requested() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let other_workspace = tempfile::tempdir().expect("other workspace");
    let config = memory_config(home.path());
    let runtime = runtime(home.path());
    let (service, user, usable) =
        otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new()).expect("open");
    assert!(usable);
    let current = otto::runtime::wiring::workspace_memory_scope(
        &runtime,
        &workspace.path().to_string_lossy(),
    )
    .expect("current scope");
    let other = otto::runtime::wiring::workspace_memory_scope(
        &runtime,
        &other_workspace.path().to_string_lossy(),
    )
    .expect("other scope");
    for (scope, key) in [
        (user, "user-record"),
        (current, "current-record"),
        (other, "other-record"),
    ] {
        service
            .remember(&RememberRequest {
                scope,
                kind: "note".into(),
                key: key.into(),
                text: key.into(),
                ..RememberRequest::default()
            })
            .expect("remember");
    }
    service.close().expect("close");
    let cwd = workspace.path().to_string_lossy().into_owned();
    let (code, default_output, stderr) =
        memory(&["list", "--config", &config, "--cwd", &cwd], home.path());
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(default_output.contains("user-record"));
    assert!(default_output.contains("current-record"));
    assert!(!default_output.contains("other-record"));
    let (code, output, stderr) = memory(
        &[
            "list", "--scope", "all", "--limit", "1", "--config", &config, "--cwd", &cwd,
        ],
        home.path(),
    );
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(output.contains("next_cursor="), "output = {output:?}");
}

#[test]
fn forget_removes_a_record_from_the_workspace_scope() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = otto::runtime::sandbox_runtime::canonical_directory(workspace.path())
        .expect("canonical workspace");
    let config = memory_config(home.path());

    let runtime = runtime(home.path());
    let (service, _, usable) =
        otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the store must be usable");
    let scope =
        otto::runtime::wiring::workspace_memory_scope(&runtime, &workspace_path.to_string_lossy())
            .expect("workspace scope");
    let record = service
        .remember(&RememberRequest {
            scope: scope.clone(),
            kind: "preference".into(),
            key: "editor".into(),
            text: "vim".into(),
            ..RememberRequest::default()
        })
        .expect("remember");
    service.close().expect("close");

    let (code, stdout, stderr) = memory(
        &[
            "forget",
            &record.id,
            "--config",
            &config,
            "--cwd",
            &workspace_path.to_string_lossy(),
        ],
        home.path(),
    );
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains(&record.id), "stdout = {stdout:?}");

    let (reopened, _, usable) =
        otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("reopen memory service");
    assert!(usable, "the reopened store must be usable");
    let reference = RecordRef {
        scope,
        id: record.id,
    };
    assert!(
        reopened.get(&reference).is_err(),
        "the forgotten record must be gone"
    );
    reopened.close().expect("close");
}

#[test]
fn forget_without_an_id_reports_usage() {
    let home = tempfile::tempdir().expect("home");
    let config = memory_config(home.path());
    let (code, _, stderr) = memory(&["forget", "--config", &config], home.path());
    assert_ne!(code, 0);
    assert!(stderr.contains("usage"), "stderr = {stderr:?}");
}

#[test]
fn no_subcommand_reports_usage() {
    let home = tempfile::tempdir().expect("home");
    let (code, _, stderr) = memory(&[], home.path());
    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        concat!(
            "otto: usage: otto memory status|list|show <id>|forget <id>|history <id>|diff <id> <from> [<to>]|restore <id> <revision> ",
            "[--scope current|user|workspace|all] [--limit N] [--cursor CURSOR] [--config PATH] [--cwd PATH]\n"
        )
    );
}

#[test]
fn an_unknown_subcommand_fails() {
    let home = tempfile::tempdir().expect("home");
    let (code, _, stderr) = memory(&["bogus"], home.path());
    assert_ne!(code, 0);
    assert!(stderr.contains("bogus"), "stderr = {stderr:?}");
}

#[test]
fn config_and_resolve_failures_use_fixed_diagnostics() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let cwd = workspace.path().to_string_lossy().into_owned();
    let missing = home
        .path()
        .join("config-secret")
        .join("missing.toml")
        .to_string_lossy()
        .into_owned();
    let invalid = home.path().join("invalid-memory.toml");
    std::fs::write(
        &invalid,
        "[memory]\n[memory.turso]\nbusy_timeout = \"busy-timeout-secret\"\n",
    )
    .expect("write config");
    let invalid = invalid.to_string_lossy().into_owned();

    for (config, want, avoid) in [
        (
            &missing,
            "otto: load config: configuration is invalid or unavailable\n",
            "config-secret",
        ),
        (
            &invalid,
            "otto: memory configuration is invalid or unavailable\n",
            "busy-timeout-secret",
        ),
    ] {
        let (code, _, stderr) = memory(&["status", "--config", config, "--cwd", &cwd], home.path());
        assert_eq!((code, stderr.as_str()), (1, want));
        assert!(!stderr.contains(avoid), "stderr leaked {avoid:?}");
    }
}

#[test]
fn an_unusable_working_directory_uses_a_fixed_diagnostic() {
    let home = tempfile::tempdir().expect("home");
    let config = memory_config(home.path());
    let cwd = home
        .path()
        .join("cwd-secret-does-not-exist")
        .to_string_lossy()
        .into_owned();
    let (code, _, stderr) = memory(
        &["forget", "rec-1", "--config", &config, "--cwd", &cwd],
        home.path(),
    );
    assert_eq!(
        (code, stderr.as_str()),
        (
            1,
            "otto: resolve cwd: working directory is invalid or unavailable\n"
        )
    );
    assert!(!stderr.contains("cwd-secret"), "stderr = {stderr:?}");
}

#[test]
fn a_failing_open_uses_a_fixed_diagnostic() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let config = home.path().join("encrypted.toml");
    std::fs::write(
        &config,
        "[memory]\nenabled = true\nrequire_encryption = true\n",
    )
    .expect("write config");
    let (code, _, stderr) = memory(
        &[
            "status",
            "--config",
            &config.to_string_lossy(),
            "--cwd",
            &workspace.path().to_string_lossy(),
        ],
        home.path(),
    );
    assert_eq!(
        (code, stderr.as_str()),
        (1, "otto: memory backend is unavailable\n")
    );
}

/// The status writer is called directly with a path that cannot be opened
/// and a secret value that covers the directory holding it.
#[test]
fn status_redacts_the_configured_path_and_reports_the_warning() {
    let home = tempfile::tempdir().expect("home");
    let secret = home.path().join("acct-secret");
    std::fs::write(&secret, b"not a directory").expect("write blocker");
    let secret = secret.to_string_lossy().into_owned();
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: format!("{secret}/memory"),
        ..MemoryRuntime::default()
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = status(
        &otto::runtime::storage::Storage::default(),
        &runtime,
        std::slice::from_ref(&secret),
        ".",
        &mut stdout,
        &mut stderr,
    );
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    assert_eq!(code, 0, "stderr = {:?}", String::from_utf8_lossy(&stderr));
    assert!(
        !stdout.contains(&secret),
        "stdout leaked the path: {stdout:?}"
    );
    for want in [
        "enabled: true",
        "backend: files",
        "path: ",
        "/memory",
        "usable: false",
        STORE_UNAVAILABLE_WARNING,
    ] {
        assert!(stdout.contains(want), "stdout = {stdout:?}, want {want:?}");
    }
}

#[test]
fn a_plugin_source_refuses_otto_memory_and_opens_no_file_store() {
    let home = tempfile::tempdir().expect("home");
    let config = home.path().join("otto.toml");
    std::fs::write(&config, "[memory]\nbackend = \"plugin:memfix\"\n").expect("write config");
    let (code, stdout, stderr) = memory(
        &["status", "--config", &config.to_string_lossy()],
        home.path(),
    );
    assert_eq!(code, 1);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "otto: memory source plugin:memfix does not support otto memory; the plugin manages its records\n"
    );
    assert!(!store_dir(home.path()).exists());
}
