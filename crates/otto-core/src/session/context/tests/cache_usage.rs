use super::*;
use crate::agent::compaction::combine_compaction_usage;

#[test]
fn pi_round_trip_preserves_cache_partitions_and_explicit_zero_presence() {
    for written in [None, Some(0), Some(30)] {
        for one_hour in [None, Some(0)] {
            if written.is_none() && one_hour.is_some() {
                continue;
            }
            let usage = Usage {
                input_tokens: 100,
                output_tokens: 3,
                cached_input_tokens: 60,
                cache_write_input_tokens: written,
                cache_write_1h_input_tokens: one_hour,
            };
            let message = Message {
                role: Role::Assistant,
                blocks: vec![Block::text("answer")],
                created_at: DateTime::from_timestamp(1, 0).unwrap(),
                finish_reason: Some(FinishReason::Stop),
                usage: Some(usage),
                ..Default::default()
            };
            let (encoded, _) =
                model_message_to_pi_entry(&message, "000000a1", None, &Header::default()).unwrap();
            let wire = encoded.message.as_ref().unwrap().usage.as_ref().unwrap();
            assert_eq!(wire.input, 40 - written.unwrap_or(0));
            assert_eq!(wire.cache_write, written.unwrap_or(0));
            assert_eq!(wire.cache_write_1h, one_hour);
            assert_eq!(wire.total_tokens, 103);
            assert_eq!(
                pi_entry_to_context_messages(&encoded).unwrap()[0].usage,
                Some(usage)
            );
            let (resolved, _) = build_context(&[encoded], "000000a1").unwrap();
            assert_eq!(resolved.usage, usage);
        }
    }
    let zero = Usage {
        cache_write_input_tokens: Some(0),
        ..Default::default()
    };
    let message = Message {
        role: Role::Assistant,
        blocks: vec![Block::text("answer")],
        created_at: DateTime::from_timestamp(1, 0).unwrap(),
        finish_reason: Some(FinishReason::Stop),
        usage: Some(zero),
        ..Default::default()
    };
    let (encoded, _) =
        model_message_to_pi_entry(&message, "000000c3", None, &Header::default()).unwrap();
    assert_eq!(
        pi_entry_to_context_messages(&encoded).unwrap()[0].usage,
        Some(zero)
    );
}

#[test]
fn mismatched_usage_details_cannot_override_recorded_pi_counts() {
    let usage = Usage {
        input_tokens: 100,
        cache_write_input_tokens: Some(30),
        ..Default::default()
    };
    let message = Message {
        role: Role::Assistant,
        blocks: vec![Block::text("answer")],
        created_at: DateTime::from_timestamp(1, 0).unwrap(),
        finish_reason: Some(FinishReason::Stop),
        usage: Some(usage),
        ..Default::default()
    };
    let (mut encoded, _) =
        model_message_to_pi_entry(&message, "000000a1", None, &Header::default()).unwrap();
    encoded
        .message
        .as_mut()
        .unwrap()
        .usage
        .as_mut()
        .unwrap()
        .cache_write = 31;
    assert!(pi_entry_to_context_messages(&encoded).is_err());
}

#[test]
fn context_and_compaction_totals_keep_only_complete_write_breakdowns() {
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 3,
        cached_input_tokens: 60,
        cache_write_input_tokens: Some(30),
        cache_write_1h_input_tokens: Some(10),
    };
    let mut entries = Vec::new();
    for (id, parent, report) in [
        ("000000a1", None, usage),
        ("000000b2", Some("000000a1"), usage),
    ] {
        let message = Message {
            role: Role::Assistant,
            blocks: vec![Block::text("answer")],
            created_at: DateTime::from_timestamp(1, 0).unwrap(),
            finish_reason: Some(FinishReason::Stop),
            usage: Some(report),
            ..Default::default()
        };
        entries.push(
            model_message_to_pi_entry(&message, id, parent, &Header::default())
                .unwrap()
                .0,
        );
    }
    let (resolved, _) = build_context(&entries, "000000b2").unwrap();
    assert_eq!(resolved.usage.cache_write_input_tokens, Some(60));
    assert_eq!(resolved.usage.cache_write_1h_input_tokens, Some(20));
    let combined = combine_compaction_usage(Usage::default(), false, usage, true);
    assert_eq!(combined, (usage, true));
    let combined = combine_compaction_usage(usage, true, usage, true);
    assert_eq!(combined.0.cache_write_input_tokens, Some(60));
    let unknown = combine_compaction_usage(usage, true, Usage::default(), true);
    assert_eq!(unknown.0.cache_write_input_tokens, None);
}
