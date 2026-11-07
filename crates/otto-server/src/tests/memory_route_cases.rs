//! `/v1/sessions/{id}/memory/*` route tests against a file memory store.

use super::*;
use otto::memory::{CandidateAction, Origin, ProposeRequest, Provenance};

fn memory_harness(store: &tempfile::TempDir) -> Harness {
    Harness::with(HarnessOptions {
        memory_store: Some(store.path().join("memory")),
        ..HarnessOptions::default()
    })
}

async fn remember(harness: &Harness, id: &str, scope: &str, text: &str) -> serde_json::Value {
    let body = serde_json::json!({"scope": scope, "kind": "preference", "key": text, "text": text})
        .to_string();
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/memory/records"),
            Some(&body),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    reply.json()
}

/// Queues a model-origin `retract` candidate against `target` in the
/// workspace scope, as a `forget`/`retract` proposal from the model would.
fn propose_retract(harness: &Harness, id: &str, target: &serde_json::Value) -> String {
    let session = harness.server.lookup(id).expect("session");
    let (service, _, workspace) = session.ctrl.memory_manager().expect("memory");
    let candidates = service
        .propose(&ProposeRequest {
            action: CandidateAction::Retract,
            scope: workspace,
            target_id: target["id"].as_str().expect("id").into(),
            base_revision: target["revision"].as_u64().expect("revision"),
            reason: "the user corrected it".into(),
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        })
        .expect("propose");
    candidates[0].id.clone()
}

#[tokio::test]
async fn remember_then_show_list_and_search_return_the_record() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let record = remember(&harness, &id, "workspace", "prefers tabs").await;
    assert_eq!(record["state"], "active");
    assert_eq!(record["origin"], "human");
    assert_eq!(record["revision"], 1);
    assert_eq!(record["effective"], true);
    assert_eq!(record["scope"]["namespace"], "workspace");
    let record_id = record["id"].as_str().expect("id");

    let shown = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/memory/records/{record_id}"),
            None,
        )
        .await;
    assert_eq!(shown.status, StatusCode::OK);
    assert_eq!(shown.json()["text"], "prefers tabs");

    let listed = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/memory/records?scope=workspace&limit=10"),
            None,
        )
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.json()["records"][0]["id"], record_id);
    assert_eq!(listed.json()["next_cursor"], "");

    let found = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/memory/search?q=tabs"),
            None,
        )
        .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.body);
    assert_eq!(found.json()["records"][0]["id"], record_id);
}

#[tokio::test]
async fn the_record_list_pages_with_next_cursor_and_filters_by_kind() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let first = remember(&harness, &id, "workspace", "uses cargo nextest").await;
    let second = remember(&harness, &id, "workspace", "uses rustfmt").await;
    let records = format!("/v1/sessions/{id}/memory/records");

    let page_one = harness
        .send("GET", &format!("{records}?scope=workspace&limit=1"), None)
        .await;
    assert_eq!(page_one.status, StatusCode::OK, "{}", page_one.body);
    let page_one = page_one.json();
    assert_eq!(page_one["records"].as_array().expect("records").len(), 1);
    let cursor = page_one["next_cursor"].as_str().expect("cursor");
    assert!(!cursor.is_empty());
    let cursor: String = url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
    let page_two = harness
        .send(
            "GET",
            &format!("{records}?scope=workspace&limit=1&cursor={cursor}"),
            None,
        )
        .await;
    assert_eq!(page_two.status, StatusCode::OK, "{}", page_two.body);
    let page_two = page_two.json();
    assert_eq!(page_two["next_cursor"], "");
    let mut listed = vec![
        page_one["records"][0]["id"].clone(),
        page_two["records"][0]["id"].clone(),
    ];
    let mut expected = vec![first["id"].clone(), second["id"].clone()];
    listed.sort_by_key(|value| value.to_string());
    expected.sort_by_key(|value| value.to_string());
    assert_eq!(listed, expected);

    let other_kind = harness
        .send("GET", &format!("{records}?scope=all&kind=fact"), None)
        .await;
    assert_eq!(other_kind.status, StatusCode::OK, "{}", other_kind.body);
    assert_eq!(other_kind.json()["records"], serde_json::json!([]));
}

#[tokio::test]
async fn verify_returns_a_needs_review_record_to_active() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let record = remember(&harness, &id, "user", "on-call is weekly").await;
    let base = format!(
        "/v1/sessions/{id}/memory/records/{}",
        record["id"].as_str().expect("id")
    );
    let flagged = harness
        .send(
            "POST",
            &format!("{base}/state"),
            Some(r#"{"state":"needs_review","reason":"rota changed?"}"#),
        )
        .await;
    assert_eq!(flagged.status, StatusCode::OK, "{}", flagged.body);
    assert_eq!(flagged.json()["state"], "needs_review");

    let verified = harness
        .send(
            "POST",
            &format!("{base}/verify"),
            Some(r#"{"expected_revision":2}"#),
        )
        .await;
    assert_eq!(verified.status, StatusCode::OK, "{}", verified.body);
    let verified = verified.json();
    assert_eq!(verified["state"], "active");
    assert_eq!(verified["state_reason"], "");
    assert_eq!(verified["revision"], 3);
    assert!(verified["verified_at"].is_string());
}

#[tokio::test]
async fn retracting_a_record_hides_it_from_the_default_list_and_records_history() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let record = remember(&harness, &id, "user", "uses vim").await;
    let record_id = record["id"].as_str().expect("id");
    let base = format!("/v1/sessions/{id}/memory/records/{record_id}");

    let body = r#"{"state":"retracted","reason":"switched editors","expected_revision":1}"#;
    let retracted = harness
        .send("POST", &format!("{base}/state"), Some(body))
        .await;
    assert_eq!(retracted.status, StatusCode::OK, "{}", retracted.body);
    let retracted = retracted.json();
    assert_eq!(retracted["state"], "retracted");
    assert_eq!(retracted["state_reason"], "switched editors");
    assert_eq!(retracted["revision"], 2);
    assert_eq!(retracted["effective"], false);

    let active = harness
        .send("GET", &format!("/v1/sessions/{id}/memory/records"), None)
        .await;
    assert_eq!(active.json()["records"], serde_json::json!([]));
    let retracted_list = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/memory/records?state=retracted"),
            None,
        )
        .await;
    assert_eq!(retracted_list.json()["records"][0]["id"], record_id);

    let history = harness.send("GET", &format!("{base}/history"), None).await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.body);
    let revisions = history.json()["revisions"].clone();
    assert_eq!(revisions[0]["state"], "active");
    assert_eq!(revisions[1]["state"], "retracted");

    let diff = harness
        .send("GET", &format!("{base}/diff?from=1"), None)
        .await;
    assert_eq!(diff.status, StatusCode::OK, "{}", diff.body);
    assert_eq!(diff.json()["from"]["revision"], 1);
    assert_eq!(diff.json()["to"]["revision"], 2);

    let restored = harness
        .send(
            "POST",
            &format!("{base}/restore"),
            Some(r#"{"revision":1}"#),
        )
        .await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);
    assert_eq!(restored.json()["state"], "active");
    assert_eq!(restored.json()["revision"], 3);
}

#[tokio::test]
async fn a_stale_expected_revision_is_409_and_leaves_the_record_unchanged() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let record = remember(&harness, &id, "workspace", "deploys on fridays").await;
    let record_id = record["id"].as_str().expect("id");
    let base = format!("/v1/sessions/{id}/memory/records/{record_id}");
    let verified = harness
        .send(
            "POST",
            &format!("{base}/verify"),
            Some(r#"{"review_in":"30d"}"#),
        )
        .await;
    assert_eq!(verified.status, StatusCode::OK, "{}", verified.body);
    assert_eq!(verified.json()["revision"], 2);
    assert!(verified.json()["review_at"].is_string());

    let stale = harness
        .send(
            "POST",
            &format!("{base}/state"),
            Some(r#"{"state":"retracted","expected_revision":1}"#),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.json()["error"]["code"], "memory_conflict");
    let stale_forget = harness
        .send("DELETE", &format!("{base}?expected_revision=1"), None)
        .await;
    assert_eq!(
        stale_forget.status,
        StatusCode::CONFLICT,
        "{}",
        stale_forget.body
    );

    let current = harness.send("GET", &base, None).await.json();
    assert_eq!(current["state"], "active");
    assert_eq!(current["revision"], 2);
}

#[tokio::test]
async fn supersede_requires_an_active_replacement() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let old = remember(&harness, &id, "workspace", "node 18").await;
    let new = remember(&harness, &id, "workspace", "node 22").await;
    let base = format!(
        "/v1/sessions/{id}/memory/records/{}",
        old["id"].as_str().expect("id")
    );

    let missing = harness
        .send(
            "POST",
            &format!("{base}/state"),
            Some(r#"{"state":"superseded"}"#),
        )
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    let body = serde_json::json!({"state": "superseded", "superseded_by": new["id"]}).to_string();
    let superseded = harness
        .send("POST", &format!("{base}/state"), Some(&body))
        .await;
    assert_eq!(superseded.status, StatusCode::OK, "{}", superseded.body);
    assert_eq!(superseded.json()["state"], "superseded");
    assert_eq!(superseded.json()["superseded_by"], new["id"]);
}

#[tokio::test]
async fn forgetting_a_record_returns_the_tombstone_and_show_is_404() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let record = remember(&harness, &id, "workspace", "temporary").await;
    let path = format!(
        "/v1/sessions/{id}/memory/records/{}",
        record["id"].as_str().expect("id")
    );

    let forgotten = harness.send("DELETE", &path, None).await;
    assert_eq!(forgotten.status, StatusCode::OK, "{}", forgotten.body);
    assert_eq!(forgotten.json()["previous_revision"], 1);
    assert_eq!(forgotten.json()["tombstone"]["id"], record["id"]);
    let gone = harness.send("GET", &path, None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.json()["error"]["code"], "not_found");
}

#[tokio::test]
async fn a_pending_retract_candidate_is_listed_and_accepting_it_retracts_the_target() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let record = remember(&harness, &id, "workspace", "prefers yarn").await;
    let candidate_id = propose_retract(&harness, &id, &record);

    let pending = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/memory/candidates?limit=5"),
            None,
        )
        .await;
    assert_eq!(pending.status, StatusCode::OK, "{}", pending.body);
    let candidate = pending.json()["candidates"][0].clone();
    assert_eq!(candidate["id"], candidate_id.as_str());
    assert_eq!(candidate["action"], "retract");
    assert_eq!(candidate["target_id"], record["id"]);
    assert_eq!(candidate["origin"], "model");

    let review_path = format!("/v1/sessions/{id}/memory/candidates/{candidate_id}/review");
    let accepted = harness
        .send("POST", &review_path, Some(r#"{"decision":"accept"}"#))
        .await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    assert_eq!(accepted.json()["decision"], "accept");
    assert_eq!(accepted.json()["candidate"]["state"], "accepted");
    assert_eq!(accepted.json()["record"]["state"], "retracted");
    assert_eq!(
        accepted.json()["record"]["state_reason"],
        "the user corrected it"
    );

    let again = harness
        .send("POST", &review_path, Some(r#"{"decision":"reject"}"#))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);
    let empty = harness
        .send("GET", &format!("/v1/sessions/{id}/memory/candidates"), None)
        .await;
    assert_eq!(empty.json()["candidates"], serde_json::json!([]));
}

#[tokio::test]
async fn propose_without_a_proposal_generator_is_unavailable() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let record = remember(&harness, &id, "workspace", "ci runs nightly").await;
    let path = format!(
        "/v1/sessions/{id}/memory/records/{}/propose",
        record["id"].as_str().expect("id")
    );
    let reply = harness
        .send("POST", &path, Some(r#"{"evidence":"ci now runs hourly"}"#))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.body
    );
    assert_eq!(reply.json()["error"]["code"], "memory_unavailable");
}

#[tokio::test]
async fn invalid_requests_are_400_and_unknown_ids_are_404() {
    let store = tempfile::tempdir().expect("store");
    let harness = memory_harness(&store);
    let id = harness.create().await;
    let records = format!("/v1/sessions/{id}/memory/records");
    for (method, path, body) in [
        ("GET", format!("{records}?state=bogus"), None),
        ("GET", format!("{records}?limit=0"), None),
        (
            "GET",
            format!("{records}?unverified&verified_after=1d"),
            None,
        ),
        ("GET", format!("{records}?color=red"), None),
        (
            "POST",
            records.clone(),
            Some(r#"{"scope":"org","text":"x"}"#),
        ),
        (
            "POST",
            records.clone(),
            Some(r#"{"scope":"user","text":"  "}"#),
        ),
        (
            "POST",
            format!("{records}/r1/verify"),
            Some(r#"{"review_in":"2026-01-01T00:00:00Z"}"#),
        ),
        ("GET", format!("{records}/r1/diff"), None),
        ("GET", format!("/v1/sessions/{id}/memory/search"), None),
        (
            "POST",
            format!("/v1/sessions/{id}/memory/candidates/c1/review"),
            Some(r#"{"decision":"maybe"}"#),
        ),
    ] {
        let reply = harness.send(method, &path, body).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{method} {path}: {}",
            reply.body
        );
    }
    for (method, path, body) in [
        ("GET", format!("{records}/missing"), None),
        (
            "POST",
            format!("{records}/missing/state"),
            Some(r#"{"state":"retracted"}"#),
        ),
        (
            "POST",
            format!("/v1/sessions/{id}/memory/candidates/missing/review"),
            Some(r#"{"decision":"accept"}"#),
        ),
        ("GET", "/v1/sessions/nope/memory/records".to_string(), None),
    ] {
        let reply = harness.send(method, &path, body).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{method} {path}: {}",
            reply.body
        );
    }
}

#[tokio::test]
async fn a_session_without_memory_answers_503() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send("GET", &format!("/v1/sessions/{id}/memory/records"), None)
        .await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.body
    );
    assert_eq!(reply.json()["error"]["code"], "memory_unavailable");
}
