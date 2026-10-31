//! Workspace registry, trust, load and session lookup tests.

use super::*;

#[test]
fn workspaces_admit_reads_startup_and_roots() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![root_path.clone()],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    assert!(workspaces.admit(&startup_path.to_string_lossy()).is_ok());
    assert_eq!(
        workspaces.admit(&root_path.to_string_lossy()),
        Ok(root_path)
    );
}

#[test]
fn a_directory_trusted_after_startup_is_admitted_without_a_restart() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, b"").expect("write empty config");
    let trusted = tempfile::tempdir().expect("trusted");
    let trusted_path = canonical_directory(trusted.path()).expect("canonical");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: config_path.clone(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    assert_eq!(
        workspaces.admit(&trusted_path.to_string_lossy()),
        Err(Admission::NotAdmitted)
    );

    std::fs::write(
        &config_path,
        format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            trusted_path.display()
        ),
    )
    .expect("trust it");

    assert_eq!(
        workspaces.admit(&trusted_path.to_string_lossy()),
        Ok(trusted_path)
    );
}

#[tokio::test]
async fn trusting_a_workspace_writes_the_config_so_it_is_admitted() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    let picked = tempfile::tempdir().expect("picked");
    let picked_path = canonical_directory(picked.path()).expect("canonical");
    let factory = factory_with(
        &startup_path,
        sessions.path(),
        Vec::new(),
        config_path.clone(),
    );

    factory
        .trust_workspace(&picked_path.to_string_lossy())
        .await
        .expect("trusted");

    assert_eq!(
        factory.workspaces.admit(&picked_path.to_string_lossy()),
        Ok(picked_path)
    );
    assert!(
        std::fs::read_to_string(&config_path)
            .expect("config")
            .contains("trust_level = \"trusted\"")
    );
}

#[tokio::test]
async fn trusting_a_missing_directory_is_invalid() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let config_dir = tempfile::tempdir().expect("config dir");
    let factory = factory_with(
        &startup_path,
        sessions.path(),
        Vec::new(),
        config_dir.path().join("config.toml"),
    );

    let got = factory
        .trust_workspace(&startup_path.join("missing").to_string_lossy())
        .await;

    assert!(
        matches!(got, Err(server::WorkspaceLoadError::Invalid(_))),
        "{got:?}"
    );
}

#[test]
fn browse_roots_are_the_home_directory_then_the_configured_roots() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let root = tempfile::tempdir().expect("root");
    let root_path = canonical_directory(root.path()).expect("canonical");
    // `fixture_tests::builder` uses the workspace directory as home.
    let factory = factory_with(
        &startup_path,
        sessions.path(),
        vec![root_path.clone()],
        PathBuf::new(),
    );

    assert_eq!(factory.browse_roots(), vec![startup_path, root_path]);
}

#[test]
fn a_trusted_path_that_no_longer_exists_is_skipped() {
    let holder = tempfile::tempdir().expect("holder");
    let target = holder.path().join("gone");
    std::fs::create_dir(&target).expect("create");
    let target_path = canonical_directory(&target).expect("canonical");
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            target_path.display()
        ),
    )
    .expect("write config");
    std::fs::remove_dir(&target).expect("remove");

    assert_eq!(trusted_projects(&config_path), Vec::<PathBuf>::new());
}

/// Every concurrent caller loading the same path gets the same host, and
/// only one of them built it: the registry mutex is held across the
/// build, so a caller that loses the race sees the path already loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_builds_the_workspace_once_under_concurrent_callers() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let canonical = canonical_directory(workspace.path()).expect("canonical");
    let shared = crate::cli::fixture_tests::shared_with_offline_sandbox(shared_root.path());
    let runtime = crate::runtime::builder::resolve_initial_runtime(
        &shared.config,
        &shared.environment,
        None,
        &shared.overrides,
    )
    .expect("runtime");
    let workspaces = Arc::new(Workspaces {
        startup: shared.home.clone(),
        startup_host: dummy_host(Path::new(&shared.home), sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    let cancel = CancellationToken::new();

    let mut handles = Vec::new();
    for _ in 0..4 {
        let workspaces = Arc::clone(&workspaces);
        let shared = Arc::clone(&shared);
        let runtime = runtime.clone();
        let cancel = cancel.clone();
        let canonical = canonical.clone();
        handles.push(tokio::spawn(async move {
            let mut stderr = Vec::new();
            workspaces
                .load(&canonical, &shared, &runtime, &cancel, &mut stderr, false)
                .await
                .expect("load")
        }));
    }
    let mut newly_loaded_count = 0;
    let mut hosts = Vec::new();
    for handle in handles {
        let (host, newly_loaded) = handle.await.expect("join");
        if newly_loaded {
            newly_loaded_count += 1;
        }
        hosts.push(host);
    }
    assert_eq!(newly_loaded_count, 1, "exactly one caller builds the host");
    for host in &hosts[1..] {
        assert!(Arc::ptr_eq(&hosts[0], host));
    }
}

/// A failed load (here: the workspace directory does not exist) leaves
/// the registry untouched, so a later, valid load of the same path is
/// still `newly_loaded`.
#[tokio::test]
async fn a_failed_load_leaves_the_registry_unchanged() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let sessions = tempfile::tempdir().expect("sessions");
    let missing = shared_root.path().join("does-not-exist");
    let shared = crate::cli::fixture_tests::shared_with_offline_sandbox(shared_root.path());
    let runtime = crate::runtime::builder::resolve_initial_runtime(
        &shared.config,
        &shared.environment,
        None,
        &shared.overrides,
    )
    .expect("runtime");
    let workspaces = Workspaces {
        startup: shared.home.clone(),
        startup_host: dummy_host(Path::new(&shared.home), sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();

    let error = match workspaces
        .load(&missing, &shared, &runtime, &cancel, &mut stderr, false)
        .await
    {
        Ok(_) => panic!("a missing directory must fail to load"),
        Err(error) => error,
    };
    assert!(!error.is_empty());
    assert!(workspaces.loaded.lock().await.is_empty());

    std::fs::create_dir(&missing).expect("create the workspace directory");
    let (_, newly_loaded) = workspaces
        .load(&missing, &shared, &runtime, &cancel, &mut stderr, false)
        .await
        .expect("load after fixing the path");
    assert!(newly_loaded);
}

/// A session created for a named, already-loaded workspace uses that
/// workspace's own `Builder`: its `Controller::workspace()` and its
/// file-tool `Workspace` root are both that workspace, not the startup
/// one.
#[tokio::test]
async fn create_in_a_named_workspace_uses_that_workspaces_builder() {
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let startup_sessions = tempfile::tempdir().expect("startup sessions");
    let startup_host = dummy_host(&startup_path, startup_sessions.path());

    let other = tempfile::tempdir().expect("other workspace");
    let other_path = canonical_directory(other.path()).expect("canonical");
    let other_sessions = tempfile::tempdir().expect("other sessions");
    let shared = crate::cli::fixture_tests::shared_with_offline_sandbox(other_sessions.path());
    std::fs::write(&shared.config_path, "[sandbox]\ndriver='off'\n").expect("config");
    let (mut builder, sandbox) = load_workspace(
        shared,
        &other_path,
        &Disabled::default(),
        &CancellationToken::new(),
        &mut Vec::new(),
    )
    .await
    .expect("load workspace B");
    Arc::make_mut(&mut builder.shared)
        .config
        .profiles
        .get_mut("alpha")
        .unwrap()
        .model = "workspace-b-model".into();
    assert!(
        builder.grant_control.is_some(),
        "loaded workspace wires persistent read grants"
    );
    let other_host = Arc::new(WorkspaceHost {
        builder: Arc::new(builder),
        workflows: None,
        delivery_setup: match (sandbox.approval_executor, sandbox.approval_environment) {
            (Some(executor), Some(environment)) => Some(DeliverySetupCapability {
                executor,
                environment,
            }),
            _ => None,
        },
        reloader: sandbox.reloader,
    });

    let mut loaded = BTreeMap::new();
    loaded.insert(
        other_path.to_string_lossy().into_owned(),
        Arc::clone(&other_host),
    );
    let runtime = crate::cli::fixture_tests::initial_runtime(&startup_host.builder);
    let factory = ServeFactory {
        workspaces: Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host,
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(loaded),
        }),
        delivery: None,
        runtime,
        cancel: CancellationToken::new(),
    };

    let controller = factory
        .create(Some(&other_path.to_string_lossy()))
        .await
        .expect("create in workspace B");

    assert_eq!(controller.workspace(), other_path.to_string_lossy());
    assert_eq!(other_host.builder.workspace.root(), other_path);
    assert_eq!(controller.info().model, "workspace-b-model");
    controller
        .reload_sandbox()
        .await
        .expect("reload B when startup has no reloader");
    controller.close_resources().await;
    controller.close().expect("close");
    other_host
        .reloader
        .as_ref()
        .unwrap()
        .control
        .close()
        .await
        .expect("close sandbox");
}

/// Resume by id reads `<session directory>/<id>.jsonl` directly, so a
/// session older than the newest `MAX_LIST_SESSIONS` still opens.
#[tokio::test]
async fn opening_by_id_finds_a_session_outside_the_newest_listing_window() {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = canonical_directory(workspace.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let (factory, host) = factory_over(&workspace_path, sessions.path());

    use otto_core::session::Session;
    let runtime = crate::cli::fixture_tests::initial_runtime(&host.builder);
    let mut oldest = String::new();
    for index in 0..=MAX_LIST_SESSIONS {
        let session = host.builder.create_session(&runtime).expect("session");
        session
            .append(crate::cli::fixture_tests::user("hello"))
            .await
            .expect("persist the first message");
        let path = PathBuf::from(session.path());
        if index == 0 {
            oldest = path
                .file_stem()
                .expect("file stem")
                .to_string_lossy()
                .into_owned();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("session file")
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(86_400))
                .expect("age the file");
        }
    }
    let listed = listed_for(&host.builder).expect("listing");
    assert_eq!(listed.sessions.len(), MAX_LIST_SESSIONS);
    assert!(listed.sessions.iter().all(|entry| entry.id != oldest));

    let reopened = factory.open(&oldest, None).await.expect("open by id");
    assert_eq!(reopened.info().session_id, oldest);
}

/// An id that is not 32 lowercase hexadecimal characters answers "not
/// found" before it is used as a file name.
#[tokio::test]
async fn opening_a_non_hex_id_is_not_found() {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = canonical_directory(workspace.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let (factory, _host) = factory_over(&workspace_path, sessions.path());
    for id in [
        "../../etc/passwd",
        "some-id",
        &"A".repeat(32),
        &"a".repeat(31),
        &"a".repeat(33),
    ] {
        let error = match factory.open(id, None).await {
            Ok(_) => panic!("{id} opened"),
            Err(error) => error,
        };
        assert_eq!(error, SESSION_NOT_FOUND, "{id}");
    }
    assert!(sessionfs::is_session_id(&"0123456789abcdef".repeat(2)));
}

#[test]
fn a_nonexistent_root_is_a_startup_error_naming_the_key_and_path() {
    let error = canonicalize_workspace_roots(&["/does/not/exist".to_string()]).unwrap_err();
    assert!(error.contains("workspace_roots"), "{error}");
    assert!(error.contains("/does/not/exist"), "{error}");
}

#[test]
fn valid_roots_canonicalize_in_order() {
    let a = tempfile::tempdir().expect("a");
    let b = tempfile::tempdir().expect("b");
    let canonical_a = canonical_directory(a.path()).expect("canonical");
    let canonical_b = canonical_directory(b.path()).expect("canonical");
    let roots = canonicalize_workspace_roots(&[
        a.path().to_string_lossy().into_owned(),
        b.path().to_string_lossy().into_owned(),
    ])
    .expect("canonicalize");
    assert_eq!(roots, vec![canonical_a, canonical_b]);
}
