use super::*;
use crate::session::SessionHandle;
use otto_core::session::Session;

#[test]
fn local_input_events_survive_reopen_without_becoming_model_messages() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let pending = r#"{"id":"stable-first","text":"identical","state":"pending"}"#;
    let accepted = r#"{"id":"stable-first","text":"identical","state":"accepted"}"#;
    let second = r#"{"id":"stable-second","text":"identical","state":"pending"}"#;
    store.append_input_submissions(pending).unwrap();
    store.append_input_submissions(accepted).unwrap();
    store.append_input_submissions(second).unwrap();
    assert!(
        store.messages().is_empty(),
        "submission intent is not execution evidence"
    );
    assert!(store.model_messages().is_empty());
    let path = store.path();
    store.close().unwrap();
    let (reopened, _) = Store::open(&path).unwrap();
    let events: Vec<serde_json::Value> = reopened
        .input_submission_events()
        .unwrap()
        .iter()
        .map(|event| serde_json::from_str(event).unwrap())
        .collect();
    let expected: Vec<serde_json::Value> = [pending, accepted, second]
        .iter()
        .map(|event| serde_json::from_str(event).unwrap())
        .collect();
    assert_eq!(events, expected);
    assert!(reopened.messages().is_empty());
    assert!(!reopened.has_input("stable-first"));
    assert!(!reopened.has_input("stable-second"));
    assert_eq!(
        fs::read_to_string(path)
            .unwrap()
            .matches("otto.input_submissions")
            .count(),
        3
    );
}

#[tokio::test]
async fn compaction_keeps_local_input_events_on_the_complete_active_path() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("before")).unwrap();
    store
        .append_input_submissions(r#"{"id":"draft","text":"recover me"}"#)
        .unwrap();
    store.append_message(&assistant("after")).unwrap();
    store
        .append_compaction(&CompactionCheckpoint {
            summary: "summary".into(),
            first_kept_entry_id: store.messages()[1].id.clone(),
            tokens_before: 100,
            created_at: created_at(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(store.input_submission_events().unwrap().len(), 1);
    assert!(!format!("{:?}", store.model_messages()).contains("recover me"));
}
