//! Workspace registry route tests.

use super::*;

// ---- workspaces ----

#[tokio::test]
async fn listing_workspaces_answers_the_startup_workspace_alone() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/workspaces", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    let body = reply.json();
    let startup = harness.factory.workspaces.startup.clone();
    assert_eq!(body["startup"], startup);
    assert_eq!(body["roots"].as_array().expect("roots").len(), 0);
    let workspaces = body["workspaces"].as_array().expect("workspaces");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0]["path"], startup);
    assert_eq!(workspaces[0]["open_sessions"], 0);
    assert_eq!(workspaces[0]["workflows"], false);
}

#[tokio::test]
async fn registering_a_relative_path_answers_400() {
    let harness = Harness::new();
    let reply = harness
        .send_with(
            "POST",
            "/v1/workspaces",
            Some(r#"{"path":"relative"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "INVALID_WORKSPACE");
}

#[tokio::test]
async fn registering_an_unadmitted_path_answers_403() {
    let harness = Harness::new();
    let reply = harness
        .send_with(
            "POST",
            "/v1/workspaces",
            Some(r#"{"path":"/elsewhere"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_ADMITTED");
}

#[tokio::test]
async fn registering_an_admitted_path_answers_201_then_200_on_repeat() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let first = harness
        .send_with(
            "POST",
            "/v1/workspaces",
            Some(r#"{"path":"/other"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert_eq!(first.json()["path"], "/other");
    assert_eq!(first.json()["open_sessions"], 0);
    assert_eq!(first.json()["workflows"], true);

    let repeat = harness
        .send_with(
            "POST",
            "/v1/workspaces",
            Some(r#"{"path":"/other"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(repeat.status, StatusCode::OK, "{}", repeat.body);
    assert_eq!(repeat.json()["path"], "/other");

    let list = harness.send("GET", "/v1/workspaces", None).await.json();
    let workspaces = list["workspaces"].as_array().expect("workspaces");
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0]["path"], harness.factory.workspaces.startup);
    assert_eq!(workspaces[1]["path"], "/other");
}

#[tokio::test]
async fn registering_an_unadmitted_path_with_trust_records_trust_then_answers_201() {
    let harness = Harness::new();
    let reply = harness
        .send_with(
            "POST",
            "/v1/workspaces",
            Some(r#"{"path":"/elsewhere","trust":true}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(reply.json()["path"], "/elsewhere");
    assert_eq!(
        *harness.factory.workspaces.trusted.lock().expect("trusted"),
        ["/elsewhere"]
    );
}

#[tokio::test]
async fn registering_an_admitted_path_with_trust_records_nothing() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });
    let reply = harness
        .send_with(
            "POST",
            "/v1/workspaces",
            Some(r#"{"path":"/other","trust":true}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert!(
        harness
            .factory
            .workspaces
            .trusted
            .lock()
            .expect("trusted")
            .is_empty()
    );
}

#[tokio::test]
async fn browsing_directories_lists_subdirectories_of_the_first_root() {
    let home = tempfile::tempdir().expect("home");
    let root = std::fs::canonicalize(home.path()).expect("canonical");
    std::fs::create_dir(root.join("project")).expect("mkdir");
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            browse_roots: vec![root.clone()],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let reply = harness.send("GET", "/v1/fs/dirs", None).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["path"], root.to_string_lossy().as_ref());
    assert_eq!(body["dirs"][0]["name"], "project");
}

#[tokio::test]
async fn browsing_outside_the_roots_answers_403() {
    let home = tempfile::tempdir().expect("home");
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            browse_roots: vec![std::fs::canonicalize(home.path()).expect("canonical")],
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });
    // A temp directory path has no characters that need escaping.
    let query = format!("/v1/fs/dirs?path={}", elsewhere.path().display());

    let reply = harness.send("GET", &query, None).await;

    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "PATH_NOT_ALLOWED");
}

#[tokio::test]
async fn browsing_with_no_roots_answers_404() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/fs/dirs", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "BROWSE_UNAVAILABLE");
}

#[tokio::test]
async fn browsing_without_a_token_answers_401() {
    let harness = Harness::with(HarnessOptions {
        token: "secret".to_string(),
        ..HarnessOptions::default()
    });
    let reply = harness.send("GET", "/v1/fs/dirs", None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
}

#[tokio::test]
async fn a_failed_load_answers_500_with_the_real_message() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/broken".to_string()],
            error: Some(("/broken".to_string(), "sandbox open failed".to_string())),
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let reply = harness
        .send_with(
            "POST",
            "/v1/workspaces",
            Some(r#"{"path":"/broken"}"#),
            &[("content-type", b"application/json")],
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.body
    );
    assert_eq!(reply.json()["error"]["code"], "internal");
    assert_eq!(reply.json()["error"]["message"], "sandbox open failed");
}

#[tokio::test]
async fn deleting_a_loaded_workspace_unloads_it() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            loaded: tokio::sync::Mutex::new(BTreeMap::from([("/other".to_string(), true)])),
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let reply = harness
        .send("DELETE", "/v1/workspaces?path=/other", None)
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.body);

    let list = harness.send("GET", "/v1/workspaces", None).await.json();
    let workspaces = list["workspaces"].as_array().expect("workspaces");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0]["path"], harness.factory.workspaces.startup);
}

#[tokio::test]
async fn deleting_an_unknown_path_answers_404() {
    let harness = Harness::new();
    let reply = harness
        .send("DELETE", "/v1/workspaces?path=/nope", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_NOT_FOUND");
}

#[tokio::test]
async fn deleting_the_startup_workspace_answers_409() {
    let harness = Harness::new();
    let startup = harness.factory.workspaces.startup.clone();
    let reply = harness
        .send("DELETE", &format!("/v1/workspaces?path={startup}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_IS_STARTUP");
}

#[tokio::test]
async fn deleting_a_workspace_with_an_open_session_answers_409() {
    // A session registered directly against a second workspace's own
    // builder, bypassing `TestFactory` (which always builds against the
    // harness's one builder).
    let other_workspace = tempfile::tempdir().expect("other workspace");
    let other_sessions = tempfile::tempdir().expect("other sessions");
    // Canonical, as a real session's workspace is (`/var` → `/private/var`).
    let other_root = std::fs::canonicalize(other_workspace.path()).expect("canonical");
    let other_builder = Arc::new(fixture_tests::builder(&other_root, other_sessions.path()));
    let other_path = other_builder.workspace_path.clone();

    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec![other_path.clone()],
            loaded: tokio::sync::Mutex::new(BTreeMap::from([(other_path.clone(), true)])),
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let session = SharedSession::memory(Header {
        version: CURRENT_VERSION,
        id: new_id().expect("id"),
        workspace: other_path.clone(),
        provider: "openai-compatible".to_string(),
        profile: "alpha".to_string(),
        model: "test-model".to_string(),
        created_at: chrono::Utc::now(),
    });
    let runner = Runner::scripted(
        session.clone(),
        Arc::clone(&harness.provider) as Arc<dyn Provider + Send + Sync>,
        Arc::new(crate::subagent::tasks::Tasks::new()),
    );
    let other_ctrl = Controller::with_builder(
        other_builder,
        true,
        session,
        runner,
        RuntimeInfo {
            provider: "alpha".to_string(),
            provider_type: "openai-compatible".to_string(),
            model_selector: "alpha/test-model".to_string(),
            model: "test-model".to_string(),
            thinking: "high".to_string(),
            context_window: 128_000,
            sandbox: SandboxInfo::default(),
        },
    );
    harness.server.register(other_ctrl);

    let reply = harness
        .send("DELETE", &format!("/v1/workspaces?path={other_path}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_IN_USE");

    // A non-canonical spelling of the same directory is checked too.
    let reply = harness
        .send(
            "DELETE",
            &format!("/v1/workspaces?path={other_path}/."),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);

    let list = harness.send("GET", "/v1/workspaces", None).await.json();
    assert_eq!(list["workspaces"].as_array().expect("workspaces").len(), 2);
}

#[tokio::test]
async fn deleting_a_workspace_with_an_active_run_answers_409() {
    let harness = Harness::with(HarnessOptions {
        workspaces: FakeWorkspaces {
            admitted: vec!["/other".to_string()],
            loaded: tokio::sync::Mutex::new(BTreeMap::from([("/other".to_string(), true)])),
            in_use: Some("/other".to_string()),
            ..FakeWorkspaces::default()
        },
        ..HarnessOptions::default()
    });

    let reply = harness
        .send("DELETE", "/v1/workspaces?path=/other", None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "WORKSPACE_IN_USE");

    let list = harness.send("GET", "/v1/workspaces", None).await.json();
    assert_eq!(list["workspaces"].as_array().expect("workspaces").len(), 2);
}

#[tokio::test]
async fn deleting_a_workspace_without_a_token_answers_401() {
    let harness = Harness::with(HarnessOptions {
        token: "secret".to_string(),
        ..HarnessOptions::default()
    });
    let reply = harness
        .send("DELETE", "/v1/workspaces?path=/other", None)
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
}
