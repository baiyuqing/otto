use super::*;
use crate::usage::Context;

fn report(written: Option<i64>, one_hour: Option<i64>) -> UsageRecord {
    UsageRecord {
        occurred_at: chrono::Utc::now().to_rfc3339(),
        context: Context {
            session_id: "cache".into(),
            provider: "claude".into(),
            ..Default::default()
        },
        kind: "provider",
        usage_present: true,
        cached_input_tokens_present: true,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 3,
            cached_input_tokens: 40,
            cache_write_input_tokens: written,
            cache_write_1h_input_tokens: one_hour,
        },
    }
}

#[test]
fn write_aggregates_require_complete_reports_and_preserve_zero_across_empty_days() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.summary(None).unwrap().cache_write_input_tokens, None);
    store.append(&report(Some(0), Some(0))).unwrap();
    let zero = store.summary(None).unwrap();
    assert_eq!(zero.cache_write_input_tokens, Some(0));
    assert_eq!(zero.cache_write_1h_input_tokens, Some(0));
    store
        .append(&UsageRecord {
            usage_present: false,
            cached_input_tokens_present: false,
            usage: Usage::default(),
            ..report(None, None)
        })
        .unwrap();
    let with_absent = store.summary(None).unwrap();
    assert_eq!(with_absent.cache_write_input_tokens, Some(0));
    let daily = store.daily(2, None).unwrap();
    assert_eq!(daily.summary, with_absent);
    assert_eq!(daily.daily[0].cache_write_input_tokens, None);
    store.append(&report(Some(20), Some(5))).unwrap();
    let complete = store.summary(None).unwrap();
    assert_eq!(complete.cache_write_input_tokens, Some(20));
    assert_eq!(complete.cache_write_1h_input_tokens, Some(5));
    assert_eq!(store.daily(2, None).unwrap().summary, complete);
    store.append(&report(Some(10), None)).unwrap();
    let partial = store.summary(None).unwrap();
    assert_eq!(partial.cache_write_input_tokens, Some(30));
    assert_eq!(partial.cache_write_1h_input_tokens, None);
    assert_eq!(store.daily(2, None).unwrap().summary, partial);
    store.append(&report(None, None)).unwrap();
    let unknown = store.summary(None).unwrap();
    assert_eq!(unknown.cache_write_input_tokens, None);
    assert_eq!(unknown.cache_write_1h_input_tokens, None);
    assert_eq!(store.daily(2, None).unwrap().summary, unknown);
}

#[test]
fn invalid_cache_partitions_are_rejected_at_both_append_and_sql_import_boundaries() {
    let store = Store::open_in_memory().unwrap();
    for (written, one_hour) in [
        (Some(-1), None),
        (Some(61), None),
        (Some(5), Some(6)),
        (None, Some(0)),
    ] {
        assert!(store.append(&report(written, one_hour)).is_err());
    }
    store.append(&report(None, None)).unwrap();
    let connection = store.connection.lock().unwrap();
    for statement in [
        "UPDATE usage_events SET cache_write_input_tokens=61",
        "UPDATE usage_events SET cache_write_1h_input_tokens=0",
        "UPDATE usage_events SET cache_write_input_tokens=5, cache_write_1h_input_tokens=6",
        "UPDATE usage_events SET usage_present=0, input_tokens=0, output_tokens=0, cached_input_tokens=0, cache_write_input_tokens=0",
    ] {
        assert!(connection.execute_batch(statement).is_err(), "{statement}");
    }
}
