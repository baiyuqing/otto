//! Reflection, generated skill and skill manager route tests.

use super::*;

// ---- reflection ----

#[tokio::test]
async fn reflecting_an_unknown_session_is_404_and_a_bad_body_is_400() {
    let harness = Harness::new();
    let reply = harness
        .send("POST", "/v1/sessions/missing/reflect", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let id = harness.create().await;
    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/reflect"), Some("{nope"))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reflecting_while_a_turn_runs_is_409_turn_active() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let (_stream, _turn_id) = start_stream(&harness, &id).await;
    harness.provider.wait_started(1).await;

    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/reflect"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "turn_active");
    gate.cancel();
}

#[tokio::test]
async fn reflecting_a_session_that_cannot_reflect_is_409_unavailable_and_frees_the_slot() {
    // The test controllers keep their history in memory, so there is no
    // session file to read.
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/reflect"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "reflection_unavailable");
    // The compaction slot is released, so a compaction is not refused.
    let reply = harness
        .send("POST", &format!("/v1/sessions/{id}/compact"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

#[tokio::test]
async fn canceling_reflection_before_it_registers_cancels_that_request_id_only() {
    let harness = Harness::new();
    let id = harness.create().await;
    let cancel = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect/cancel"),
            Some(r#"{"request_id":"reflect-1"}"#),
        )
        .await;
    assert_eq!(cancel.status, StatusCode::ACCEPTED, "{}", cancel.body);

    let canceled = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect"),
            Some(r#"{"request_id":"reflect-1"}"#),
        )
        .await;
    assert_eq!(canceled.status, StatusCode::OK, "{}", canceled.body);
    assert!(canceled.body.is_empty());

    let other = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect"),
            Some(r#"{"request_id":"reflect-2"}"#),
        )
        .await;
    assert_eq!(other.status, StatusCode::CONFLICT, "{}", other.body);
    assert_eq!(other.json()["error"]["code"], "reflection_unavailable");
}

#[tokio::test]
async fn invalid_reflection_request_ids_are_rejected_without_binding() {
    let harness = Harness::new();
    let id = harness.create().await;
    let invalid = "a".repeat(65);
    let cancel = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect/cancel"),
            Some(&format!(r#"{{"request_id":"{invalid}"}}"#)),
        )
        .await;
    assert_eq!(cancel.status, StatusCode::BAD_REQUEST, "{}", cancel.body);

    let valid = "a".repeat(64);
    let reflect = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect"),
            Some(&format!(r#"{{"request_id":"{valid}"}}"#)),
        )
        .await;
    assert_eq!(reflect.status, StatusCode::CONFLICT, "{}", reflect.body);
    assert_eq!(reflect.json()["error"]["code"], "reflection_unavailable");

    let control = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect"),
            Some(r#"{"request_id":"bad!"}"#),
        )
        .await;
    assert_eq!(control.status, StatusCode::BAD_REQUEST, "{}", control.body);
}

#[tokio::test]
async fn canceling_reflection_matches_only_the_active_request_id() {
    let harness = Harness::new();
    let id = harness.create().await;
    let active = CancellationToken::new();
    let other = CancellationToken::new();
    let session = harness.server.lookup(&id).expect("session");
    session.lock().reflecting = Some(ActiveReflection {
        request_id: Some("active".to_string()),
        cancel: active.clone(),
    });

    let stale = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect/cancel"),
            Some(r#"{"request_id":"stale"}"#),
        )
        .await;
    assert_eq!(stale.status, StatusCode::ACCEPTED, "{}", stale.body);
    assert!(!active.is_cancelled());

    session.lock().reflecting = Some(ActiveReflection {
        request_id: Some("other".to_string()),
        cancel: other.clone(),
    });
    let matching = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect/cancel"),
            Some(r#"{"request_id":"other"}"#),
        )
        .await;
    assert_eq!(matching.status, StatusCode::ACCEPTED, "{}", matching.body);
    assert!(other.is_cancelled());
}

#[tokio::test]
async fn stale_reflection_cancel_does_not_cancel_later_compaction() {
    let harness = Harness::new();
    let id = harness.create().await;
    let compaction = CancellationToken::new();
    let session = harness.server.lookup(&id).expect("session");
    {
        let mut state = session.lock();
        state.remember_reflection_cancel("done");
        state.compacting = Some(compaction.clone());
    }

    let stale = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/reflect/cancel"),
            Some(r#"{"request_id":"done"}"#),
        )
        .await;
    assert_eq!(stale.status, StatusCode::ACCEPTED, "{}", stale.body);
    assert!(!compaction.is_cancelled());
}

/// A reflection store, a home directory, and the skill roots under it.
pub(super) struct ReflectionHome {
    _home: TempDir,
    path: std::path::PathBuf,
    store: Arc<crate::reflection::Store>,
    roots: crate::reflection::skillwrite::Roots,
}

pub(super) fn reflection_home() -> ReflectionHome {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().to_path_buf();
    let roots = crate::reflection::skill_roots(
        &path.to_string_lossy(),
        &[path.join(".otto/skills").to_string_lossy().into_owned()],
    )
    .expect("roots");
    ReflectionHome {
        _home: home,
        path,
        store: Arc::new(crate::reflection::Store::open_in_memory().expect("store")),
        roots,
    }
}

pub(super) fn write_generated_skill(
    home: &ReflectionHome,
    name: &str,
    body: &str,
    action_revise: bool,
) {
    use crate::reflection::guard::{self, Candidate, Verdict};
    use crate::reflection::output::SkillAction;
    let candidate = Candidate {
        action: if action_revise {
            SkillAction::Revise
        } else {
            SkillAction::Create
        },
        name: name.into(),
        description: "Run the lint gate before committing".into(),
        body: body.into(),
        scripts: Default::default(),
        reason: "the user asked for it and it ran cleanly".into(),
        cited: Vec::new(),
    };
    let entries = HashMap::new();
    let checked = guard::check(candidate, &entries, &|text: &str| text.to_owned()).expect("check");
    let vetted = guard::approve(checked, Verdict::NotRequested);
    crate::reflection::skillwrite::apply(
        home.store.as_ref(),
        &home.roots,
        &vetted,
        "run-7",
        "session-9",
        "2026-10-02T00:00:00Z",
        30,
    )
    .expect("write");
}

pub(super) const GENERATED_BODY: &str = "1. Run `cargo fmt --all`.\n2. Run `cargo clippy --workspace -- -D warnings`.\n3. Fix every warning before committing.";

#[tokio::test]
async fn skill_manager_global_state_tracks_disk_without_changing_session_catalog() {
    for startup_enabled in [false, true] {
        let home = reflection_home();
        write_generated_skill(&home, "lint-gate", GENERATED_BODY, false);
        let harness = Harness::with(HarnessOptions {
            reflection: Some((Arc::clone(&home.store), home.path.clone())),
            config_text: Some(format!("[skills]\nenabled = {startup_enabled}\n")),
            skills_enabled: Some(startup_enabled),
            ..HarnessOptions::default()
        });
        let id = harness.create().await;
        let base = format!("/v1/sessions/{id}");
        for global_enabled in [startup_enabled, !startup_enabled, startup_enabled] {
            std::fs::write(
                &harness.factory.builder.config_path,
                format!("[skills]\nenabled = {global_enabled}\n"),
            )
            .unwrap();
            let reply = harness
                .send("GET", &format!("{base}/skill-manager"), None)
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
            let json = reply.json();
            assert_eq!(json["global_enabled"], global_enabled);
            assert_eq!(json["skills"].as_array().unwrap().len(), 1);
            assert_eq!(json["skills"][0]["skill"]["name"], "lint-gate");
            assert_eq!(
                json["skills"][0]["enabled"], true,
                "per-skill preference is independent"
            );
            assert_eq!(
                json["skills"][0]["loaded"], startup_enabled,
                "fixed session catalog"
            );
        }
    }
}

#[tokio::test]
async fn skill_manager_keeps_disabled_discovered_skills_and_confirms_persistence_over_http() {
    let home = reflection_home();
    write_generated_skill(&home, "lint-gate", GENERATED_BODY, false);
    let harness = Harness::with(HarnessOptions {
        reflection: Some((Arc::clone(&home.store), home.path.clone())),
        config_text: Some("[skills]\ndisabled = [\"lint-gate\"]\n".into()),
        disabled_skills: vec!["lint-gate".into()],
        ..HarnessOptions::default()
    });
    // Match the real startup's resolved configuration; config_text alone is a disk fixture.
    let catalog = harness
        .factory
        .builder
        .build_catalogs(&mut Vec::new(), 4096, &mut Vec::new())
        .expect("catalogs");
    assert!(catalog.skills.lookup("lint-gate").is_none());
    assert!(catalog.discovered_skills.lookup("lint-gate").is_some());
    let id = harness.create().await;
    let base = format!("/v1/sessions/{id}");
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["skills"][0]["skill"]["name"], "lint-gate");
    assert_eq!(reply.json()["skills"][0]["enabled"], false);
    let loaded = reply.json()["skills"][0]["loaded"].clone();
    assert_eq!(loaded, false, "disabled skill must not load into session");
    let reply = harness
        .send("GET", &format!("{base}/skill-manager/lint-gate"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(
        reply.json()["body"]
            .as_str()
            .unwrap()
            .contains("cargo fmt --all"),
        "{}",
        reply.body
    );
    assert!(
        reply.json()["skill"]["path"]
            .as_str()
            .unwrap()
            .ends_with("SKILL.md")
    );
    let reply = harness
        .send("POST", &format!("{base}/skills/lint-gate/enable"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["restart_required"], true);
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.json()["skills"][0]["enabled"], true);
    assert_eq!(
        reply.json()["skills"][0]["loaded"],
        loaded,
        "never hot reload"
    );
    let reply = harness
        .send("POST", &format!("{base}/skills/lint-gate/disable"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.json()["skills"][0]["enabled"], false);
    assert_eq!(reply.json()["skills"].as_array().unwrap().len(), 1);
    std::fs::write(&harness.factory.builder.config_path, "not valid TOML = [").unwrap();
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.json()["error"]["code"], "skill_config_read_failed");
    let reply = harness
        .send("POST", &format!("{base}/skills/lint-gate/enable"), None)
        .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.json()["error"]["code"], "skill_config_write_failed");
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/missing/skill-manager", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_generated_skills_list_says_when_reflection_is_off() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/reflection/skills"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["enabled"], false);
    assert_eq!(reply.json()["skills"], serde_json::json!([]));
    let reply = harness
        .send("GET", "/v1/sessions/missing/reflection/skills", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reflection_run_logs_list_and_show_over_http() {
    let home = reflection_home();
    let harness = Harness::with(HarnessOptions {
        reflection: Some((Arc::clone(&home.store), home.path.clone())),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let list = format!("/v1/sessions/{id}/reflection/runs");
    let reply = harness.send("GET", &list, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.json(),
        serde_json::json!({"enabled": true, "runs": []})
    );

    let dir = home.path.join(".otto/reflection/runs");
    crate::reflection::runlog::write(
            &dir,
            "2026-10-05T02:03:00Z",
            "abcd1234-run",
            "# Reflection run abcd1234-run\n\n- trigger: requested\n- status: ok\n- started: 2026-10-05T02:03:00Z\n\n## Retrospective\n\n> Read the docs first.\n",
        )
        .expect("write a log");
    let reply = harness.send("GET", &list, None).await;
    let runs = &reply.json()["runs"];
    assert_eq!(runs.as_array().expect("array").len(), 1, "{runs}");
    assert_eq!(runs[0]["id"], "abcd1234");
    assert_eq!(runs[0]["trigger"], "requested");
    assert_eq!(runs[0]["headline"], "Read the docs first.");

    let reply = harness.send("GET", &format!("{list}/abcd1234"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(
        reply.json()["content"]
            .as_str()
            .expect("content")
            .contains("> Read the docs first.")
    );
    for missing in ["zzzz9999", "..%2Fsessions"] {
        let reply = harness
            .send("GET", &format!("{list}/{missing}"), None)
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{missing}");
    }
}

#[tokio::test]
async fn generated_skills_list_and_revert_over_http() {
    let home = reflection_home();
    write_generated_skill(&home, "lint-gate", GENERATED_BODY, false);
    write_generated_skill(&home, "fmt-check", GENERATED_BODY, false);
    let harness = Harness::with(HarnessOptions {
        reflection: Some((Arc::clone(&home.store), home.path.clone())),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let list = format!("/v1/sessions/{id}/reflection/skills");

    let reply = harness.send("GET", &list, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["enabled"], true);
    let names: Vec<_> = body["skills"]
        .as_array()
        .expect("array")
        .iter()
        .map(|skill| skill["name"].as_str().expect("name").to_owned())
        .collect();
    assert_eq!(names, ["fmt-check", "lint-gate"]);
    assert_eq!(body["skills"][1]["owned"], true);
    assert_eq!(body["skills"][1]["run_id"], "run-7");
    assert_eq!(body["skills"][1]["session_id"], "session-9");

    // Reverting a skill reflection created removes it.
    let reply = harness
        .send("POST", &format!("{list}/lint-gate/revert"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["result"], "removed");
    assert!(!home.path.join(".otto/skills/lint-gate/SKILL.md").exists());
    // It is gone from the list and a second revert is 404.
    let reply = harness.send("GET", &list, None).await;
    assert_eq!(reply.json()["skills"].as_array().expect("array").len(), 1);
    let reply = harness
        .send("POST", &format!("{list}/lint-gate/revert"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // A skill edited by hand is listed as not owned, and revert refuses it.
    let edited = home.path.join(".otto/skills/fmt-check/SKILL.md");
    std::fs::write(&edited, "my own version").expect("edit");
    let reply = harness.send("GET", &list, None).await;
    assert_eq!(reply.json()["skills"][0]["owned"], false);
    let reply = harness
        .send("POST", &format!("{list}/fmt-check/revert"), None)
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "skill_not_owned");
    assert_eq!(
        std::fs::read_to_string(&edited).expect("read"),
        "my own version"
    );
}

#[tokio::test]
async fn reverting_restores_the_previous_version_and_rejects_bad_names() {
    let home = reflection_home();
    write_generated_skill(&home, "lint-gate", GENERATED_BODY, false);
    let first =
        std::fs::read_to_string(home.path.join(".otto/skills/lint-gate/SKILL.md")).expect("first");
    write_generated_skill(
        &home,
        "lint-gate",
        &format!("{GENERATED_BODY}\n4. Re-run the tests."),
        true,
    );
    let harness = Harness::with(HarnessOptions {
        reflection: Some((Arc::clone(&home.store), home.path.clone())),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    let base = format!("/v1/sessions/{id}/reflection/skills");

    let reply = harness
        .send("POST", &format!("{base}/lint-gate/revert"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["result"], "restored");
    assert_eq!(
        std::fs::read_to_string(home.path.join(".otto/skills/lint-gate/SKILL.md")).expect("read"),
        first
    );
    let reply = harness
        .send("POST", &format!("{base}/Not%20A%20Name/revert"), None)
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    let reply = harness
        .send(
            "POST",
            "/v1/sessions/missing/reflection/skills/x/revert",
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn notices_are_listed_by_id_without_being_removed() {
    let harness = Harness::new();
    let id = harness.create().await;
    let path = format!("/v1/sessions/{id}/notices");
    let reply = harness.send("GET", &path, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), serde_json::json!({"notices": [], "last": 0}));

    let session = harness.server.lookup(&id).expect("session");
    session.ctrl.push_notice("reflection: first");
    session.ctrl.push_routed_notice(
        "reflection: second",
        crate::schedule::Origin::chat("telegram", "42").unwrap(),
    );

    let reply = harness.send("GET", &path, None).await;
    assert_eq!(
        reply.json(),
        serde_json::json!({
            "notices": [
                {"id": 1, "text": "reflection: first"},
                {"id": 2, "text": "reflection: second", "presentation_origin": "chat:telegram:42"}
            ],
            "last": 2,
        })
    );
    // A second client, or a repeated read, sees them again.
    assert_eq!(
        harness.send("GET", &path, None).await.json()["notices"]
            .as_array()
            .expect("array")
            .len(),
        2
    );
    let reply = harness.send("GET", &format!("{path}?after=1"), None).await;
    assert_eq!(
        reply.json()["notices"],
        serde_json::json!([{
            "id": 2,
            "text": "reflection: second",
            "presentation_origin": "chat:telegram:42"
        }])
    );
    let reply = harness.send("GET", &format!("{path}?after=2"), None).await;
    assert_eq!(reply.json(), serde_json::json!({"notices": [], "last": 2}));
    let reply = harness
        .send("GET", "/v1/sessions/missing/notices", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}
