use super::*;

#[test]
fn session_and_compaction_cache_writes_survive_reopening_without_rewriting_history() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 3,
        cached_input_tokens: 60,
        cache_write_input_tokens: Some(30),
        cache_write_1h_input_tokens: Some(10),
    };
    store.append_message(&user("question")).unwrap();
    store
        .append_message(&assistant_with_usage("answer", usage))
        .unwrap();
    store.append_message(&user("next question")).unwrap();
    let path = store.path();
    let original = fs::read(&path).unwrap();
    let anchor = message_entry_id(&path, 3);
    let zero = Usage {
        cache_write_input_tokens: Some(0),
        ..Default::default()
    };
    let mut checkpoint = checkpoint(&anchor);
    checkpoint.usage = Some(zero);
    store.append_compaction(&checkpoint).unwrap();
    let after = fs::read(&path).unwrap();
    assert!(after.starts_with(&original));
    store.close().unwrap();
    let (reopened, warnings) = Store::open(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reopened.latest_compaction().unwrap().usage, Some(zero));
    assert_eq!(reopened.messages()[0].usage, Some(zero));
    assert_eq!(fs::read(&path).unwrap(), after);
    reopened.close().unwrap();
}
