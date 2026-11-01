use super::*;
mod delivery_manager;
mod delivery_run;
mod persisted_workspaces;
mod workspace_registry;

const TEST_SERVER_TOKEN: &str = "test-server-token-0123456789abcdefghijklmnop";

#[test]
fn tcp_startup_prints_connection_details() {
    let mut stdout = Vec::new();
    announce_listen(&mut stdout, "http", "127.0.0.1:8787", Some("tok"));
    assert_eq!(
        String::from_utf8(stdout).expect("utf-8"),
        "otto serve: http://127.0.0.1:8787/?token=tok\n"
    );
}

#[test]
fn an_environment_token_is_not_printed() {
    let mut stdout = Vec::new();
    announce_listen(&mut stdout, "https", "127.0.0.1:8787", None);
    assert_eq!(
        String::from_utf8(stdout).expect("utf-8"),
        "otto serve: https://127.0.0.1:8787/ (token from OTTO_SERVER_TOKEN)\n"
    );
}

// ---- OTTO_SERVER_TOKEN and bind ----

#[test]
fn server_token_length_and_characters_are_validated() {
    assert!(validate_server_token(&"a".repeat(31)).is_err());
    assert!(validate_server_token(&"a".repeat(32)).is_ok());
    assert!(validate_server_token(&"a".repeat(4000)).is_ok());
    assert!(validate_server_token(&"a".repeat(4001)).is_err());
    assert!(validate_server_token(&format!("{} b", "a".repeat(32))).is_err());
    assert!(validate_server_token(&format!("{}\u{7f}", "a".repeat(32))).is_err());
}

/// Writes a self-signed certificate and key for `localhost` into `directory`.
fn self_signed_tls_files(directory: &tempfile::TempDir) -> otto_core::config::ServerTls {
    let identity =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("certificate");
    let cert_path = directory.path().join("cert.pem");
    let key_path = directory.path().join("key.pem");
    std::fs::write(&cert_path, identity.cert.pem()).expect("write certificate");
    std::fs::write(&key_path, identity.signing_key.serialize_pem()).expect("write key");
    otto_core::config::ServerTls {
        cert_path: cert_path.to_string_lossy().into_owned(),
        key_path: key_path.to_string_lossy().into_owned(),
    }
}

fn tcp_runtime(address: &str, tls: Option<otto_core::config::ServerTls>) -> ServerRuntime {
    ServerRuntime {
        listen: address.into(),
        tls,
        ..Default::default()
    }
}

fn discard_logger() -> Arc<Logger> {
    Arc::new(Logger::discard())
}

#[tokio::test]
async fn a_bad_environment_token_fails_bind() {
    let error = bind(
        &tcp_runtime("127.0.0.1:0", None),
        "short",
        &discard_logger(),
    )
    .err()
    .expect("bind must fail");
    assert!(error.contains("OTTO_SERVER_TOKEN"), "{error}");
}

#[tokio::test]
async fn a_non_loopback_address_needs_tls_and_an_environment_token() {
    let directory = tempfile::tempdir().expect("directory");
    let tls = self_signed_tls_files(&directory);
    for (runtime, token) in [
        (tcp_runtime("0.0.0.0:0", None), TEST_SERVER_TOKEN),
        (tcp_runtime("0.0.0.0:0", Some(tls)), ""),
    ] {
        let error = bind(&runtime, token, &discard_logger())
            .err()
            .expect("bind must fail");
        assert!(error.contains("[server].tls_cert"), "{error}");
    }
}

#[tokio::test]
async fn tls_with_a_random_token_binds_loopback_only() {
    let directory = tempfile::tempdir().expect("directory");
    let tls = self_signed_tls_files(&directory);
    let bound = bind(
        &tcp_runtime("127.0.0.1:0", Some(tls)),
        "",
        &discard_logger(),
    )
    .expect("bind");
    assert!(bound.tls);
    assert!(!bound.remote);
    assert!(!bound.token_from_env);
    assert_eq!(bound.token.len(), 32);
}

#[tokio::test]
async fn tls_and_an_environment_token_bind_a_non_loopback_address() {
    let directory = tempfile::tempdir().expect("directory");
    let tls = self_signed_tls_files(&directory);
    let bound = bind(
        &tcp_runtime("0.0.0.0:0", Some(tls)),
        TEST_SERVER_TOKEN,
        &discard_logger(),
    )
    .expect("bind");
    assert!(bound.remote);
    assert!(bound.tls);
    assert!(bound.token_from_env);
    assert_eq!(bound.token, TEST_SERVER_TOKEN);
    assert!(matches!(bound.tcp, Some(Listener::Tls(_))));
}

// ---- admission ----

#[test]
fn the_startup_workspace_is_admitted_with_no_roots() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let admitted = admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[], &[])
        .expect("admitted");
    assert_eq!(admitted, startup_path);
}

#[test]
fn a_root_itself_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let admitted = admit_workspace(
        &root_path.to_string_lossy(),
        &startup_path,
        std::slice::from_ref(&root_path),
        &[],
    )
    .expect("admitted");
    assert_eq!(admitted, root_path);
}

#[test]
fn a_descendant_of_a_root_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let child = root_path.join("project");
    std::fs::create_dir(&child).expect("child");
    let admitted =
        admit_workspace(&child.to_string_lossy(), &startup_path, &[root_path], &[]).expect("ok");
    assert_eq!(admitted, child);
}

#[test]
fn a_sibling_of_a_root_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let parent = tempfile::tempdir().expect("parent");
    let root = parent.path().join("root");
    std::fs::create_dir(&root).expect("root");
    let root_path = canonical_directory(&root).expect("canonical");
    let sibling = parent.path().join("sibling");
    std::fs::create_dir(&sibling).expect("sibling");
    assert_eq!(
        admit_workspace(&sibling.to_string_lossy(), &startup_path, &[root_path], &[]),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_prefix_lookalike_sibling_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let parent = tempfile::tempdir().expect("parent");
    let root = parent.path().join("root");
    std::fs::create_dir(&root).expect("root");
    let root_path = canonical_directory(&root).expect("canonical");
    let lookalike = parent.path().join("root-x");
    std::fs::create_dir(&lookalike).expect("lookalike");
    assert_eq!(
        admit_workspace(
            &lookalike.to_string_lossy(),
            &startup_path,
            &[root_path],
            &[]
        ),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_symlink_inside_a_root_pointing_outside_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let outside = tempfile::tempdir().expect("outside");
    let outside_path = canonical_directory(outside.path()).expect("canonical");
    let link = root_path.join("escape");
    std::os::unix::fs::symlink(&outside_path, &link).expect("symlink");
    assert_eq!(
        admit_workspace(&link.to_string_lossy(), &startup_path, &[root_path], &[]),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_missing_path_is_invalid() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let missing = startup.path().join("does-not-exist");
    assert_eq!(
        admit_workspace(&missing.to_string_lossy(), &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
}

#[test]
fn a_file_is_invalid_not_a_directory() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let file = startup.path().join("file.txt");
    std::fs::write(&file, b"hi").expect("write");
    assert_eq!(
        admit_workspace(&file.to_string_lossy(), &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
}

#[test]
fn a_relative_path_is_invalid() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    assert_eq!(
        admit_workspace(".", &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
    assert_eq!(
        admit_workspace("rel/dir", &startup_path, &[], &[]),
        Err(Admission::Invalid)
    );
}

#[test]
fn empty_roots_admits_only_the_startup_path() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let other = tempfile::tempdir().expect("other");
    let other_path = canonical_directory(other.path()).expect("canonical");
    assert!(admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[], &[]).is_ok());
    assert_eq!(
        admit_workspace(&other_path.to_string_lossy(), &startup_path, &[], &[]),
        Err(Admission::NotAdmitted)
    );
}

#[test]
fn a_trusted_directory_outside_every_root_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let trusted = tempfile::tempdir().expect("trusted");
    let trusted_path = canonical_directory(trusted.path()).expect("canonical");
    let admitted = admit_workspace(
        &trusted_path.to_string_lossy(),
        &startup_path,
        &[],
        std::slice::from_ref(&trusted_path),
    )
    .expect("admitted");
    assert_eq!(admitted, trusted_path);
}

#[test]
fn a_descendant_of_a_trusted_directory_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let trusted = tempfile::tempdir().expect("trusted");
    let trusted_path = canonical_directory(trusted.path()).expect("canonical");
    let child = trusted_path.join("project");
    std::fs::create_dir(&child).expect("child");
    let admitted = admit_workspace(
        &child.to_string_lossy(),
        &startup_path,
        &[],
        &[trusted_path],
    )
    .expect("ok");
    assert_eq!(admitted, child);
}

#[test]
fn a_prefix_lookalike_sibling_of_a_trusted_directory_is_not_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let parent = tempfile::tempdir().expect("parent");
    let trusted = parent.path().join("app");
    std::fs::create_dir(&trusted).expect("trusted");
    let trusted_path = canonical_directory(&trusted).expect("canonical");
    let lookalike = parent.path().join("app-x");
    std::fs::create_dir(&lookalike).expect("lookalike");
    assert_eq!(
        admit_workspace(
            &lookalike.to_string_lossy(),
            &startup_path,
            &[],
            &[trusted_path]
        ),
        Err(Admission::NotAdmitted)
    );
}

/// A `WorkspaceHost` for tests that need one but never open a real
/// sandbox or workflow controller.
fn dummy_host(workspace_root: &Path, session_root: &Path) -> Arc<WorkspaceHost> {
    Arc::new(WorkspaceHost {
        builder: Arc::new(otto::fixtures::builder(workspace_root, session_root)),
        workflows: None,
        delivery_setup: None,
        reloader: None,
    })
}

fn approval_for_delivery(
    id: &str,
    spec: &otto::tool::ApprovalSpec,
) -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: Vec::new(),
        schema_version: 1,
        id: id.into(),
        revision: 1,
        kind: otto_core::approval::Kind::Authorization,
        authority: otto_core::approval::Authority::Boundary,
        scope: otto_core::approval::Scope {
            workspace: "/source".into(),
            session_id: "source-session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
            tool_call_id: "delivery-call".into(),
            source: "delivery".into(),
            id: "op_delivery".into(),
            version: spec.version.clone(),
            details: spec.details.clone(),
        },
        title: spec.title.clone(),
        reason: spec.reason.clone(),
        persistent: false,
        blocking: true,
        created_at: 1,
        expires_at: Some(2),
        state: otto_core::approval::State::Approved,
        decision: None,
        application: otto_core::approval::ApplicationState::Applying,
        application_error: String::new(),
        review: None,
    }
}

fn factory_with(
    startup: &Path,
    sessions: &Path,
    roots: Vec<PathBuf>,
    config_path: PathBuf,
) -> ServeFactory {
    let startup_host = dummy_host(startup, sessions);
    let runtime = otto::fixtures::initial_runtime(&startup_host.builder);
    ServeFactory {
        workspaces: Arc::new(Workspaces {
            startup: startup.to_string_lossy().into_owned(),
            startup_host,
            roots,
            config_path,
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        }),
        delivery: None,
        runtime,
        cancel: CancellationToken::new(),
    }
}

fn factory_over(startup_path: &Path, sessions: &Path) -> (ServeFactory, Arc<WorkspaceHost>) {
    let startup_host = dummy_host(startup_path, sessions);
    let runtime = otto::fixtures::initial_runtime(&startup_host.builder);
    let factory = ServeFactory {
        workspaces: Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: Arc::clone(&startup_host),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::from([(
                startup_path.to_string_lossy().into_owned(),
                Arc::clone(&startup_host),
            )])),
        }),
        delivery: None,
        runtime,
        cancel: CancellationToken::new(),
    };
    (factory, startup_host)
}

// ---- persisted workspace list ----

fn runtime_for(shared: &Arc<otto::runtime::builder::Shared>) -> Runtime {
    otto::runtime::builder::resolve_initial_runtime(
        &shared.config,
        &shared.environment,
        None,
        &shared.overrides,
    )
    .expect("runtime")
}
