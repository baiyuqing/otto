//! Persisted workspace list tests.

use super::*;

/// Adding a workspace persists it, and a fresh registry over the same
/// home (a restart) reloads it, so it shows up in `Factory::workspaces`
/// the same way `GET /v1/workspaces` reports it.
#[tokio::test]
async fn a_persisted_workspace_reloads_at_startup_and_is_listed() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let roots_dir = tempfile::tempdir().expect("roots");
    let roots_path = canonical_directory(roots_dir.path()).expect("canonical");
    let workspace_path = roots_path.join("project");
    std::fs::create_dir(&workspace_path).expect("workspace directory");
    let sessions = tempfile::tempdir().expect("sessions");
    let shared = crate::fixtures::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();

    let first_run = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![roots_path.clone()],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    first_run
        .load(
            &workspace_path,
            &shared,
            &runtime,
            &cancel,
            &mut stderr,
            true,
        )
        .await
        .expect("load and persist");

    // The restart: a fresh registry seeded with only the startup
    // workspace, as `run` seeds it before reloading the persisted list.
    let restarted = Arc::new(Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![roots_path],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    });
    load_persisted_workspaces(&restarted, &shared, &runtime, &cancel, &mut stderr).await;
    let factory = ServeFactory {
        workspaces: restarted,
        delivery: None,
        runtime,
        cancel,
    };

    let listed = factory.workspaces().await.loaded;
    assert!(
        listed
            .iter()
            .any(|info| info.path == workspace_path.to_string_lossy()),
        "{listed:?}"
    );
}

/// A path saved while it was inside `workspace_roots` is skipped with a
/// warning once a config change moves it outside; a path still inside
/// loads normally.
#[tokio::test]
async fn a_listed_path_outside_current_roots_is_skipped_others_load() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let roots_dir = tempfile::tempdir().expect("roots");
    let roots_path = canonical_directory(roots_dir.path()).expect("canonical");
    let admitted = roots_path.join("project");
    std::fs::create_dir(&admitted).expect("admitted child");
    let outside = tempfile::tempdir().expect("outside");
    let outside_path = canonical_directory(outside.path()).expect("canonical");
    let shared = crate::fixtures::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let home_str = home.path().to_str().expect("utf-8 home");
    add_to_workspace_list(home_str, &admitted.to_string_lossy()).expect("add admitted");
    add_to_workspace_list(home_str, &outside_path.to_string_lossy()).expect("add outside");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: vec![roots_path],
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

    let loaded = workspaces.loaded.lock().await;
    assert!(loaded.contains_key(&admitted.to_string_lossy().into_owned()));
    assert!(!loaded.contains_key(&outside_path.to_string_lossy().into_owned()));
    drop(loaded);
    let warning = String::from_utf8(stderr).expect("utf-8");
    assert!(
        warning.contains(outside_path.to_string_lossy().as_ref()),
        "{warning}"
    );
    assert!(
        warning.contains("outside the startup workspace and configured roots"),
        "{warning}"
    );
}

/// A persisted path whose directory was since deleted is skipped with a
/// warning; startup does not fail because of it.
#[tokio::test]
async fn a_deleted_persisted_directory_is_skipped_with_a_warning() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let missing = home.path().join("does-not-exist");
    let shared = crate::fixtures::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    add_to_workspace_list(
        home.path().to_str().expect("utf-8 home"),
        &missing.to_string_lossy(),
    )
    .expect("add missing path");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

    assert!(workspaces.loaded.lock().await.is_empty());
    let warning = String::from_utf8(stderr).expect("utf-8");
    assert!(
        warning.contains(missing.to_string_lossy().as_ref()),
        "{warning}"
    );
    assert!(warning.contains("not an existing directory"), "{warning}");
}

/// An unparsable persisted file is a startup warning, not a startup
/// failure, and startup never rewrites it: only a later successful add
/// does.
#[tokio::test]
async fn an_unparsable_workspace_list_file_warns_and_is_left_unchanged() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let shared = crate::fixtures::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let home_str = home.path().to_str().expect("utf-8 home");
    std::fs::create_dir_all(home.path().join(".otto")).expect(".otto dir");
    let garbage = b"not json".to_vec();
    std::fs::write(workspace_list_path(home_str), &garbage).expect("write garbage");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

    assert!(workspaces.loaded.lock().await.is_empty());
    let warning = String::from_utf8(stderr).expect("utf-8");
    assert!(warning.contains("warning:"), "{warning}");
    let after = std::fs::read(workspace_list_path(home_str)).expect("read back");
    assert_eq!(after, garbage, "an unparsable file must be left untouched");
}

/// The startup workspace is loaded before `Workspaces` exists and is
/// already in `loaded`, so `load` never reaches the persist step for it;
/// adding a different path twice still leaves one entry.
#[tokio::test]
async fn the_startup_workspace_is_never_persisted_and_duplicate_adds_collapse() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let shared = crate::fixtures::shared_with_offline_sandbox(home.path());
    let runtime = runtime_for(&shared);
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();
    let startup_host = dummy_host(&startup_path, sessions.path());
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: Arc::clone(&startup_host),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::from([(
            startup_path.to_string_lossy().into_owned(),
            startup_host,
        )])),
    };

    let (_, newly_loaded) = workspaces
        .load(&startup_path, &shared, &runtime, &cancel, &mut stderr, true)
        .await
        .expect("load the already-loaded startup workspace");
    assert!(!newly_loaded);
    assert!(
        !workspace_list_path(home.path().to_str().expect("utf-8 home")).exists(),
        "the startup workspace must never be written to the persisted list"
    );

    let other = tempfile::tempdir().expect("other");
    let other_path = canonical_directory(other.path()).expect("canonical");
    let other_string = other_path.to_string_lossy().into_owned();
    let home_str = home.path().to_str().expect("utf-8 home");
    add_to_workspace_list(home_str, &other_string).expect("add once");
    add_to_workspace_list(home_str, &other_string).expect("add twice");
    assert_eq!(
        read_workspace_list(home_str).expect("read"),
        vec![other_string]
    );
}

// ---- removing a workspace ----

/// Removing a loaded, persisted workspace drops its host and its
/// persisted-list entry in one call. The other entries keep their
/// original order: `remove_from_workspace_list`'s `retain` never
/// resorts, unlike `add_to_workspace_list`.
#[tokio::test]
async fn removing_a_loaded_and_persisted_workspace_unloads_it_and_updates_the_list() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = canonical_directory(workspace.path()).expect("canonical");
    let workspace_string = workspace_path.to_string_lossy().into_owned();
    let home_str = home.path().to_str().expect("utf-8 home");
    write_workspace_list(
        home_str,
        vec![
            "z-other".to_string(),
            workspace_string.clone(),
            "a-other".to_string(),
        ],
    )
    .expect("seed the persisted list");

    let mut loaded = BTreeMap::new();
    loaded.insert(
        workspace_string.clone(),
        dummy_host(&workspace_path, sessions.path()),
    );
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(loaded),
    };
    let mut stderr = Vec::new();

    workspaces
        .remove(&workspace_string, home_str, &mut stderr)
        .await
        .expect("remove a loaded workspace");

    assert!(
        !workspaces
            .loaded
            .lock()
            .await
            .contains_key(&workspace_string)
    );
    assert_eq!(
        read_workspace_list(home_str).expect("read"),
        vec!["z-other".to_string(), "a-other".to_string()],
        "the other entries keep their original order"
    );
}

/// A path only in the persisted list, never loaded, is removed from the
/// list; `loaded` is untouched.
#[tokio::test]
async fn removing_a_persisted_only_workspace_removes_it_from_the_list() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let home_str = home.path().to_str().expect("utf-8 home");
    add_to_workspace_list(home_str, "/some/persisted/path").expect("seed");
    add_to_workspace_list(home_str, "/other/path").expect("seed");

    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let mut stderr = Vec::new();

    workspaces
        .remove("/some/persisted/path", home_str, &mut stderr)
        .await
        .expect("remove a persisted-only workspace");

    assert!(workspaces.loaded.lock().await.is_empty());
    assert_eq!(
        read_workspace_list(home_str).expect("read"),
        vec!["/other/path".to_string()]
    );
}

/// A path neither loaded nor in the persisted list is `NotFound`.
#[tokio::test]
async fn removing_an_unknown_path_answers_not_found() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let mut stderr = Vec::new();
    let home_str = home.path().to_str().expect("utf-8 home");

    let error = workspaces
        .remove("/never/added", home_str, &mut stderr)
        .await
        .expect_err("an unknown path must not be removable");
    assert!(matches!(error, server::WorkspaceRemoveError::NotFound));
}

/// The startup workspace can never be removed, even when it happens to
/// also be in the persisted list.
#[tokio::test]
async fn removing_the_startup_workspace_answers_is_startup() {
    let home = tempfile::tempdir().expect("home");
    let startup = tempfile::tempdir().expect("startup");
    let startup_path = canonical_directory(startup.path()).expect("canonical");
    let sessions = tempfile::tempdir().expect("sessions");
    let workspaces = Workspaces {
        startup: startup_path.to_string_lossy().into_owned(),
        startup_host: dummy_host(&startup_path, sessions.path()),
        roots: Vec::new(),
        config_path: PathBuf::new(),
        loaded: tokio::sync::Mutex::new(BTreeMap::new()),
    };
    let mut stderr = Vec::new();
    let home_str = home.path().to_str().expect("utf-8 home");

    let error = workspaces
        .remove(&startup_path.to_string_lossy(), home_str, &mut stderr)
        .await
        .expect_err("the startup workspace must not be removable");
    assert!(matches!(error, server::WorkspaceRemoveError::IsStartup));
}
