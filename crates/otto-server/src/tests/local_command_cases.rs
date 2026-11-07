//! Local command receipts are independent session history, not model turns.
use super::*;
use otto_core::session::local_command::LocalCommandStatus;

#[tokio::test]
async fn local_command_receipts_cover_success_queries_errors_and_forget_without_messages() {
    let store = tempfile::tempdir().expect("store");
    let harness = Harness::with(HarnessOptions {
        memory_store: Some(store.path().join("memory")),
        persistent: true,
        ..Default::default()
    });
    let id = harness.create().await;
    let endpoint = format!("/v1/sessions/{id}/local-commands");
    let run = |name: &str, args: &str| serde_json::json!({"name":name,"args":args}).to_string();
    let remembered = harness
        .send(
            "POST",
            &endpoint,
            Some(&run("remember", "--scope workspace receipt-only-body")),
        )
        .await;
    assert_eq!(remembered.status, StatusCode::OK, "{}", remembered.body);
    let value = remembered.json();
    assert_eq!(value["error"], false, "{value}");
    let receipts = value["local_commands"].as_array().unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["status"], "success");
    let controller = harness.server.lookup(&id).unwrap().ctrl.clone();
    let record_id = receipts[0]["summary"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    assert!(
        receipts[0]["summary"]
            .as_str()
            .unwrap()
            .contains(&record_id)
    );
    assert!(!receipts[0].to_string().contains("receipt-only-body"));
    let state = harness
        .send(
            "POST",
            &endpoint,
            Some(&run("memory", &format!("state {record_id} retracted"))),
        )
        .await;
    assert_eq!(state.json()["error"], false, "{}", state.body);
    assert!(
        state.json()["local_commands"][1]["summary"]
            .as_str()
            .unwrap()
            .contains(&record_id)
    );
    assert!(
        state.json()["local_commands"][1]["summary"]
            .as_str()
            .unwrap()
            .contains("retracted")
    );
    let shown = harness
        .send(
            "POST",
            &endpoint,
            Some(&run("memory", &format!("show {record_id}"))),
        )
        .await;
    assert!(
        shown.json()["local_commands"][2]["output"]
            .as_str()
            .unwrap()
            .contains("receipt-only-body")
    );
    let invalid = harness
        .send("POST", &endpoint, Some(&run("memory", "not-a-command")))
        .await;
    assert_eq!(invalid.json()["local_commands"][3]["status"], "error");
    let missing = harness
        .send("POST", &endpoint, Some(&run("memory", "show absent")))
        .await;
    assert_eq!(missing.json()["local_commands"][4]["status"], "error");
    let forgotten = harness
        .send(
            "POST",
            &endpoint,
            Some(&run("memory", &format!("forget {record_id}"))),
        )
        .await;
    assert_eq!(forgotten.json()["error"], false, "{}", forgotten.body);
    let last = forgotten.json()["local_commands"][5].clone();
    assert!(last["summary"].as_str().unwrap().contains(&record_id));
    assert!(!last.to_string().contains("receipt-only-body"));
    assert!(
        controller.history().is_empty(),
        "local commands must not become model messages"
    );
    use otto_core::session::Session;
    assert!(controller.current_session().model_messages().is_empty());
    assert_eq!(controller.info().local_commands.len(), 6);
    assert_eq!(
        controller.info().local_commands[0].status,
        LocalCommandStatus::Success
    );
    let discovered = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await;
    assert_eq!(
        discovered.json()["local_commands"]
            .as_array()
            .unwrap()
            .len(),
        6
    );

    // Attached clients must stop before fetching/replaying the older receipts
    // when recording fails; an HTTP 200 error flag is not a transport failure.
    controller.current_session().handle().close().unwrap();
    let failed = harness
        .send(
            "POST",
            &endpoint,
            Some(&run("remember", "must not execute")),
        )
        .await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    let failure = failed.json();
    assert_eq!(failure["error"]["code"], "local_command_recording");
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("reconcile before retrying")
    );
    assert!(failure.get("local_commands").is_none());
    assert_eq!(controller.info().local_commands.len(), 6);
}
