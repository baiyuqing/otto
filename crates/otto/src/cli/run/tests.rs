use super::*;
use crate::runtime::sandbox_runtime::canonical_directory;
use crate::runtime::startup::{
    MAX_CAPTURED_ENVIRONMENT_BYTES, MAX_CAPTURED_ENVIRONMENT_ENTRIES, config_environment,
    load_workspace, resolve_sandbox_settings, skill_checker,
};
use otto_core::config::File;
use std::collections::HashMap;
use std::io::Cursor;

fn entries(values: &[&str]) -> Vec<Vec<u8>> {
    values
        .iter()
        .map(|value| value.as_bytes().to_vec())
        .collect()
}

/// `otto memory status` with a regular file at `.otto/storage`, where the
/// storage authority cannot be opened. Returns the exit code.
async fn memory_status(home: &std::path::Path) -> i32 {
    let args = ["memory".to_string(), "status".to_string()];
    let home_entry = format!("HOME={}", home.display());
    run(
        &args,
        Box::new(Cursor::new(Vec::new())),
        &mut Vec::new(),
        &mut Vec::new(),
        entries(&[&home_entry]),
        false,
        &CancellationToken::new(),
        &crate::cli::terminate::Terminate::new(),
    )
    .await
}

#[tokio::test]
async fn memory_commands_skip_the_storage_authority_once_the_store_exists() {
    let blocked = |home: &std::path::Path| {
        std::fs::create_dir_all(home.join(".otto")).expect("otto dir");
        std::fs::write(home.join(".otto").join("storage"), b"x").expect("blocker");
    };
    let control = tempfile::tempdir().expect("control home");
    blocked(control.path());
    assert_ne!(memory_status(control.path()).await, 0);

    let home = tempfile::tempdir().expect("home");
    blocked(home.path());
    crate::memory::files::FileStore::open(
        &home.path().join(".otto").join("memory"),
        crate::memory::repository::MemoryOptions::default(),
    )
    .expect("create store");
    assert_eq!(memory_status(home.path()).await, 0);
}

#[test]
fn the_lookup_keeps_only_well_formed_entries() {
    let lookup = environment_lookup(&entries(&[
        "HOME=/home/user",
        "no-equals",
        "1BAD=x",
        "OK_2=value",
        "EMPTY=",
    ]))
    .expect("lookup");
    assert_eq!(lookup.get("HOME").map(String::as_str), Some("/home/user"));
    assert_eq!(lookup.get("OK_2").map(String::as_str), Some("value"));
    assert_eq!(lookup.get("EMPTY").map(String::as_str), Some(""));
    assert!(!lookup.contains_key("1BAD"));
    assert!(!lookup.contains_key("no-equals"));
}

#[test]
fn an_oversized_environment_is_refused() {
    let huge = vec![b'A'; MAX_CAPTURED_ENVIRONMENT_BYTES + 1];
    assert!(capture_environment(vec![huge]).is_err());
    assert!(
        capture_environment(vec![
            b"HOME=/x".to_vec();
            MAX_CAPTURED_ENVIRONMENT_ENTRIES + 1
        ])
        .is_err()
    );
}

#[test]
fn skill_checker_is_none_when_skills_are_disabled() {
    let mut file = File::default();
    file.skills.enabled = Some(false);
    let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
    let mut stderr = Cursor::new(Vec::new());

    assert!(
        skill_checker(
            &crate::runtime::storage::Storage::default(),
            &file,
            &environment,
            "/nonexistent",
            &mut stderr,
        )
        .is_none()
    );
    assert!(stderr.get_ref().is_empty());
}

#[test]
fn skill_checker_is_none_when_explicitly_disabled() {
    let mut file = File::default();
    file.skills.typesafe_skill_check = Some(false);
    let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
    let mut stderr = Cursor::new(Vec::new());

    assert!(
        skill_checker(
            &crate::runtime::storage::Storage::default(),
            &file,
            &environment,
            "/nonexistent",
            &mut stderr,
        )
        .is_none()
    );
    assert!(stderr.get_ref().is_empty());
}

#[test]
fn skill_checker_is_none_without_an_api_key() {
    let file = File::default();
    let environment = HashMap::new();
    let mut stderr = Cursor::new(Vec::new());

    assert!(
        skill_checker(
            &crate::runtime::storage::Storage::default(),
            &file,
            &environment,
            "/nonexistent",
            &mut stderr,
        )
        .is_none()
    );
    assert!(stderr.get_ref().is_empty());
}

#[test]
fn skill_checker_is_enabled_by_default_with_an_api_key() {
    let home = tempfile::tempdir().expect("home");
    let file = File::default();
    let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
    let mut stderr = Cursor::new(Vec::new());

    let checker = skill_checker(
        &crate::runtime::storage::Storage::default(),
        &file,
        &environment,
        &home.path().to_string_lossy(),
        &mut stderr,
    );

    assert!(checker.is_some());
    assert!(stderr.get_ref().is_empty());
    assert!(home.path().join(".otto/skill-checks.db").exists());
}

#[test]
fn skill_checker_honors_the_legacy_experimental_setting() {
    let mut file = File::default();
    file.experimental.typesafe_skill_check = Some(false);
    let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
    let mut stderr = Cursor::new(Vec::new());

    assert!(
        skill_checker(
            &crate::runtime::storage::Storage::default(),
            &file,
            &environment,
            "/nonexistent",
            &mut stderr,
        )
        .is_none()
    );
}

#[test]
fn the_config_environment_always_holds_every_fixed_key() {
    let mut file = otto_core::config::File::default();
    file.profiles.insert(
        "alpha".to_string(),
        otto_core::config::Profile {
            api_key_env: "ALPHA_KEY".to_string(),
            workspace_id_env: "CLAUDE_WORKSPACE".to_string(),
            ..Default::default()
        },
    );
    let mut lookup = HashMap::new();
    lookup.insert("ALPHA_KEY".to_string(), "sk-alpha".to_string());
    lookup.insert(
        "ANTHROPIC_API_KEY".to_string(),
        "sk-ant-default".to_string(),
    );
    lookup.insert("CLAUDE_WORKSPACE".to_string(), "wrkspc-one".to_string());
    let environment = config_environment(&file, &lookup);
    for key in [
        "HOME",
        "OTTO_PROVIDER",
        "OTTO_PROFILE",
        "OTTO_MODEL",
        "OTTO_API_KEY",
        "OTTO_UI",
        "OTTO_TRACE",
    ] {
        assert_eq!(environment.get(key).map(String::as_str), Some(""), "{key}");
    }
    assert_eq!(
        environment.get("ALPHA_KEY").map(String::as_str),
        Some("sk-alpha")
    );
    assert_eq!(
        environment.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("sk-ant-default")
    );
    assert_eq!(
        environment.get("CLAUDE_WORKSPACE").map(String::as_str),
        Some("wrkspc-one")
    );
}

#[test]
fn the_config_environment_carries_the_web_search_keys() {
    let file = otto_core::config::File::default();
    let keyless = |lookup: &[(&str, &str)]| {
        let lookup: HashMap<String, String> = lookup
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let environment = config_environment(&file, &lookup);
        crate::tool::web::search_is_keyless(&|name| environment.get(name).cloned())
    };
    assert!(keyless(&[]));
    assert!(!keyless(&[("BRAVE_API_KEY", "brave")]));
    assert!(!keyless(&[
        ("GOOGLE_API_KEY", "google"),
        ("GOOGLE_SEARCH_ENGINE_ID", "cx"),
    ]));
}

#[test]
fn provider_environment_names_are_sorted_and_deduplicated() {
    let mut file = otto_core::config::File::default();
    file.profiles.insert(
        "alpha".to_string(),
        otto_core::config::Profile {
            api_key_env: "ZED_KEY".to_string(),
            workspace_id_env: "CLAUDE_WORKSPACE".to_string(),
            ..Default::default()
        },
    );
    file.profiles.insert(
        "beta".to_string(),
        otto_core::config::Profile {
            api_key_env: "OTTO_API_KEY".to_string(),
            ..Default::default()
        },
    );
    assert_eq!(
        sandbox_provider_environment_names(&file, "BETA_KEY"),
        vec![
            "ANTHROPIC_API_KEY".to_string(),
            "BETA_KEY".to_string(),
            "CLAUDE_WORKSPACE".to_string(),
            "OTTO_API_KEY".to_string(),
            "ZED_KEY".to_string()
        ]
    );
}

#[test]
fn home_comes_from_the_environment_first() {
    let mut lookup = HashMap::new();
    lookup.insert("HOME".to_string(), "/home/user".to_string());
    assert_eq!(resolve_home(&lookup).as_deref(), Ok("/home/user"));
    assert!(resolve_home(&HashMap::new()).is_ok());
}

#[test]
fn the_frontend_follows_the_ui_mode_and_the_terminal() {
    use otto_core::config::UiMode;
    assert_eq!(select_frontend(UiMode::Auto, false), Ok(Frontend::Repl));
    assert_eq!(select_frontend(UiMode::Auto, true), Ok(Frontend::Tui));
    assert_eq!(select_frontend(UiMode::Repl, true), Ok(Frontend::Repl));
    assert_eq!(select_frontend(UiMode::Repl, false), Ok(Frontend::Repl));
    assert_eq!(select_frontend(UiMode::Tui, true), Ok(Frontend::Tui));
    assert_eq!(
        select_frontend(UiMode::Tui, false),
        Err(
            "--ui tui requires terminal stdin and stdout; use --ui repl for redirected input"
                .to_string()
        )
    );
}

#[test]
fn sandbox_settings_pick_up_existing_skill_and_agent_roots() {
    let directory = tempfile::tempdir().expect("tempdir");
    let workspace = directory.path().to_string_lossy().to_string();
    std::fs::create_dir(directory.path().join(".otto")).expect("otto dir");
    std::fs::create_dir(directory.path().join(".otto/skills")).expect("skills dir");
    let file = otto_core::config::File::default();
    let settings =
        resolve_sandbox_settings(&file, &HashMap::new(), &workspace, None).expect("settings");
    let skills = directory
        .path()
        .join(".otto/skills")
        .to_string_lossy()
        .to_string();
    assert!(settings.read_paths.contains(&skills), "{settings:?}");
}

/// A workspace lives inside the sandbox's writable subtree, so anything
/// that clones or writes there controls `.otto/skills`. A symlinked root
/// would be canonicalized into an `(allow file-read* (subpath <target>))`
/// grant covering the link's target, which is how a cloned repository
/// could read `~/.ssh`. Existence is therefore checked without following
/// the link.
#[test]
fn a_symlinked_skill_root_is_not_promoted_to_a_sandbox_read_path() {
    let directory = tempfile::tempdir().expect("tempdir");
    let workspace = directory.path().to_string_lossy().to_string();
    let secrets = directory.path().join("secrets");
    std::fs::create_dir(&secrets).expect("secrets dir");
    std::fs::create_dir(directory.path().join(".otto")).expect("otto dir");
    std::os::unix::fs::symlink(&secrets, directory.path().join(".otto/skills")).expect("symlink");

    let file = otto_core::config::File::default();
    let settings =
        resolve_sandbox_settings(&file, &HashMap::new(), &workspace, None).expect("settings");

    let skills = directory
        .path()
        .join(".otto/skills")
        .to_string_lossy()
        .to_string();
    assert!(
        !settings.read_paths.contains(&skills),
        "a symlinked skill root reached the sandbox read paths: {settings:?}"
    );
}

#[tokio::test]
async fn startup_trace_environment_writes_timing_lines() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(
        home.join(".config/otto/config.toml"),
        r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"

[memory]
enabled = false

[mcp]
enabled = false

[skills]
paths = []

[agents]
paths = []

[sandbox]
driver = "off"
"#,
    )
    .expect("write config");

    let args = vec![
        "--cwd".to_string(),
        workspace.to_string_lossy().into_owned(),
        "--no-session".to_string(),
    ];
    let environment = entries(&[
        &format!("HOME={}", home.to_string_lossy()),
        "ALPHA_KEY=sk-alpha",
        "OTTO_STARTUP_TRACE=1",
    ]);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(
        &args,
        Box::new(Cursor::new(Vec::new())),
        &mut stdout,
        &mut stderr,
        environment,
        false,
        &tokio_util::sync::CancellationToken::new(),
        &crate::cli::terminate::Terminate::new(),
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("startup ready:"), "{stderr}");
    assert!(stderr.contains("startup sandbox/open:"), "{stderr}");
    assert!(stderr.contains("startup runner/build:"), "{stderr}");
    assert!(stderr.contains("startup runner/mcp:"), "{stderr}");
    assert!(
        stderr.contains("startup runner/workspace-context:"),
        "{stderr}"
    );
    assert!(!stderr.contains("startup total:"), "{stderr}");
}

#[tokio::test]
async fn an_empty_implicit_configuration_points_to_setup() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let args = vec![
        "--cwd".to_string(),
        workspace.path().to_string_lossy().into_owned(),
        "--no-session".to_string(),
    ];
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(
        &args,
        Box::new(Cursor::new(Vec::new())),
        &mut stdout,
        &mut stderr,
        entries(&[&format!("HOME={}", home.path().to_string_lossy())]),
        false,
        &tokio_util::sync::CancellationToken::new(),
        &crate::cli::terminate::Terminate::new(),
    )
    .await;

    assert_eq!(code, 1);
    assert_eq!(
        String::from_utf8_lossy(&stderr),
        "otto: no provider configured; run `otto setup` to create one, or pass --provider and --model\n"
    );
}

/// Pins every startup warning, in emission order, with the exact bytes.
#[tokio::test]
async fn startup_warnings_keep_their_order_and_text() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    for store in ["usage.db", "tasks.db", "schedule.db"] {
        std::fs::create_dir_all(home.join(".otto").join(store)).expect("blocked store");
    }
    std::fs::create_dir_all(workspace.join(".otto")).expect("project dir");
    std::fs::write(
        workspace.join(".otto/config.toml"),
        "[sandbox]\ndriver = \"seatbelt\"\n",
    )
    .expect("write project config");
    let config = home.join(".config/otto/config.toml");
    std::fs::write(
        &config,
        r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"

[memory]
enabled = false

[mcp]
enabled = false

[sandbox]
driver = "off"
"#,
    )
    .expect("write config");

    let args = vec![
        "acp".to_string(),
        "--cwd".to_string(),
        workspace.to_string_lossy().into_owned(),
    ];
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(
        &args,
        Box::new(Cursor::new(Vec::new())),
        &mut stdout,
        &mut stderr,
        entries(&[
            &format!("HOME={}", home.to_string_lossy()),
            "ALPHA_KEY=sk-alpha",
        ]),
        false,
        &tokio_util::sync::CancellationToken::new(),
        &crate::cli::terminate::Terminate::new(),
    )
    .await;

    let stderr = String::from_utf8_lossy(&stderr).replace(
        &*workspace
            .canonicalize()
            .expect("canonical")
            .to_string_lossy(),
        "<workspace>",
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        stderr,
        "warning: sandbox in <workspace>/.otto/config.toml: field is restricted to user or managed configuration\n\
             warning: usage store unavailable, continuing without usage history\n\
             warning: task history store unavailable, continuing without task recording\n\
             warning: schedule store unavailable, continuing without scheduled deliveries\n\
             warning: sandbox is off; bash runs unsandboxed as your user\n"
    );
}

#[tokio::test]
async fn an_unsafe_command_line_exits_two() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(
        &["--cwd".to_string()],
        Box::new(Cursor::new(Vec::new())),
        &mut stdout,
        &mut stderr,
        Vec::new(),
        false,
        &tokio_util::sync::CancellationToken::new(),
        &crate::cli::terminate::Terminate::new(),
    )
    .await;
    assert_eq!(code, 2);
    assert_eq!(
        String::from_utf8_lossy(&stderr),
        "otto: invalid command-line arguments\n"
    );
}

#[tokio::test]
async fn help_writes_usage_and_exits_zero() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(
        &["--help".to_string()],
        Box::new(Cursor::new(Vec::new())),
        &mut stdout,
        &mut stderr,
        Vec::new(),
        false,
        &tokio_util::sync::CancellationToken::new(),
        &crate::cli::terminate::Terminate::new(),
    )
    .await;
    assert_eq!(code, 0);
    assert!(String::from_utf8_lossy(&stdout).starts_with("Usage: otto [options]"));
    assert!(stderr.is_empty());
}

// ---- load_workspace ----

use crate::cli::fixture_tests::{shared_with_offline_sandbox, shared_with_sandbox_driver_override};

/// Loading a second workspace must not open a second memory service: the
/// service lives on `Shared`, and `load_workspace` only computes the new
/// workspace's own scope.
#[tokio::test]
async fn load_workspace_reuses_the_shared_memory_service() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace = tempfile::tempdir().expect("workspace");
    let canonical = canonical_directory(workspace.path()).expect("canonical");
    let shared = shared_with_offline_sandbox(shared_root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stderr = Vec::new();

    let (builder, _reloader) = load_workspace(
        Arc::clone(&shared),
        &canonical,
        &Disabled::default(),
        &cancel,
        &mut stderr,
    )
    .await
    .expect("load workspace");

    assert!(Arc::ptr_eq(&builder.memory.service, &shared.memory.service));
    assert_eq!(builder.workspace_path, canonical.to_string_lossy());
}

/// The new workspace opens its own sandbox with the shared `host_entries`
/// and shell, the same as startup does for its own workspace.
#[tokio::test]
async fn load_workspace_opens_its_own_sandbox() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace = tempfile::tempdir().expect("workspace");
    let canonical = canonical_directory(workspace.path()).expect("canonical");
    let shared = shared_with_offline_sandbox(shared_root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stderr = Vec::new();

    let (builder, _reloader) = load_workspace(
        Arc::clone(&shared),
        &canonical,
        &Disabled::default(),
        &cancel,
        &mut stderr,
    )
    .await
    .expect("load workspace");

    assert_eq!(
        builder.sandbox_info.mode,
        crate::runtime::info::SandboxMode::Off
    );
    assert!(builder.sandbox_info.bash_available);
    assert!(builder.command_executor.is_some());
}

/// `otto serve --sandbox off` must apply to a workspace loaded after
/// startup too: both read the same `Shared::sandbox_driver_override`
/// rather than `load_workspace` dropping it.
#[tokio::test]
async fn load_workspace_binds_a_delivery_worktree_as_its_own_file_root() {
    let root = tempfile::tempdir().expect("root");
    let primary = root.path().join("primary");
    let worktree = primary.join(".otto/delivery-worktrees/delivery-1");
    std::fs::create_dir_all(&worktree).expect("worktree");
    let primary = canonical_directory(&primary).expect("primary canonical");
    let worktree = canonical_directory(&worktree).expect("worktree canonical");
    let shared = shared_with_offline_sandbox(root.path());
    let (builder, _sandbox) = load_workspace(
        shared,
        &worktree,
        &Disabled::default(),
        &CancellationToken::new(),
        &mut Vec::new(),
    )
    .await
    .expect("load worktree");

    assert_eq!(builder.workspace_path, worktree.to_string_lossy());
    assert_eq!(builder.workspace.root(), worktree);
    assert_ne!(builder.workspace.root(), primary);
    let runtime = crate::runtime::builder::resolve_initial_runtime(
        &builder.config,
        &builder.environment,
        None,
        &builder.overrides,
    )
    .expect("runtime");
    let session = builder.create_session(&runtime).expect("delivery session");
    let runner = builder
        .build_runner_quiet(&session, &runtime)
        .await
        .expect("delivery runner");
    assert_eq!(
        runner.session().header().workspace,
        worktree.to_string_lossy()
    );
    assert_ne!(
        runner.session().header().workspace,
        primary.to_string_lossy()
    );
}

#[tokio::test]
async fn load_workspace_honors_the_shared_sandbox_driver_override() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace = tempfile::tempdir().expect("workspace");
    let canonical = canonical_directory(workspace.path()).expect("canonical");
    let shared = shared_with_sandbox_driver_override(shared_root.path(), "off");
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stderr = Vec::new();

    let (builder, _reloader) = load_workspace(
        Arc::clone(&shared),
        &canonical,
        &Disabled::default(),
        &cancel,
        &mut stderr,
    )
    .await
    .expect("load workspace");

    assert_eq!(
        builder.sandbox_info.mode,
        crate::runtime::info::SandboxMode::Off
    );
}

/// A workspace loaded at runtime under Seatbelt gets the same elevated
/// `/approve` executor startup builds for its own workspace. Skips only
/// when `sandbox-exec` itself cannot run on this host; once it can, a
/// runtime that does not come up is a failure.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn load_workspace_seatbelt_gets_bash_approvals() {
    use std::os::unix::fs::PermissionsExt as _;

    if let Some(reason) = crate::sandbox::seatbelt::driver::unavailable_reason() {
        eprintln!("skipping: {reason}");
        return;
    }
    let shared_root = tempfile::tempdir().expect("shared root");
    // The Seatbelt private tree is created under `$HOME/Library/Caches`;
    // without it the driver does not open.
    let caches = shared_root.path().join("Library/Caches");
    std::fs::create_dir_all(&caches).expect("cache base");
    for directory in [caches.parent().expect("library"), caches.as_path()] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .expect("cache mode");
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let canonical = canonical_directory(workspace.path()).expect("canonical");
    let shared = shared_with_sandbox_driver_override(shared_root.path(), "seatbelt");
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut stderr = Vec::new();

    let (builder, sandbox) = load_workspace(
        Arc::clone(&shared),
        &canonical,
        &Disabled::default(),
        &cancel,
        &mut stderr,
    )
    .await
    .expect("load workspace");

    assert_eq!(
        builder.sandbox_info.mode,
        crate::runtime::info::SandboxMode::Seatbelt,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(builder.bash_approvals.is_some());
    assert!(sandbox.reloader.is_some());
    assert!(sandbox.approval_executor.is_some());
    assert!(sandbox.approval_environment.is_some());
    assert_eq!(
        sandbox.approval_executor.is_some(),
        sandbox.approval_environment.is_some()
    );
}
