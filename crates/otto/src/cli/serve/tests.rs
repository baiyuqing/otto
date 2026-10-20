use super::*;
mod delivery_manager;
mod delivery_run;
mod persisted_workspaces;
mod workspace_registry;

#[test]
fn tcp_startup_prints_connection_details() {
    let mut stdout = Vec::new();
    announce_listen(&mut stdout, "127.0.0.1:8787", "tok");
    assert_eq!(
        String::from_utf8(stdout).expect("utf-8"),
        "otto serve: http://127.0.0.1:8787/?token=tok\n"
    );
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
        builder: Arc::new(crate::cli::fixture_tests::builder(
            workspace_root,
            session_root,
        )),
        workflows: None,
        delivery_setup: None,
        reloader: None,
    })
}

fn approval_for_delivery(
    id: &str,
    spec: &crate::tool::ApprovalSpec,
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
    let runtime = crate::cli::fixture_tests::initial_runtime(&startup_host.builder);
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
    let runtime = crate::cli::fixture_tests::initial_runtime(&startup_host.builder);
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

fn runtime_for(shared: &Arc<crate::runtime::builder::Shared>) -> Runtime {
    crate::runtime::builder::resolve_initial_runtime(
        &shared.config,
        &shared.environment,
        None,
        &shared.overrides,
    )
    .expect("runtime")
}
