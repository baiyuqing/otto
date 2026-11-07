use super::*;

#[test]
fn turn_summary_preserves_cache_write_counts_and_does_not_invent_missing_breakdowns() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 2,
        cached_input_tokens: 40,
        cache_write_input_tokens: Some(20),
        cache_write_1h_input_tokens: Some(5),
    };
    emit(Event::ProviderUsage {
        usage,
        present: true,
        cached_input_tokens_present: true,
    });
    assert_eq!(turn.summary().usage, usage);
    emit(Event::Notification {
        input_id: String::new(),
        input_provenance: None,
        kind: None,
        task_id: "child".into(),
        text: "done".into(),
        usage,
        present: true,
        cached_input_tokens_present: true,
    });
    let total = turn.summary().usage;
    assert_eq!(total.input_tokens, 200);
    assert_eq!(total.cache_write_input_tokens, Some(40));
    assert_eq!(total.cache_write_1h_input_tokens, Some(10));
    let serialized = serde_json::to_value(turn.summary()).unwrap();
    assert_eq!(serialized["usage"]["cache_write_input_tokens"], 40);
    emit(Event::ProviderUsage {
        usage: Usage::default(),
        present: true,
        cached_input_tokens_present: false,
    });
    let unknown = turn.summary().usage;
    assert_eq!(unknown.cache_write_input_tokens, None);
    assert_eq!(unknown.cache_write_1h_input_tokens, None);
}
