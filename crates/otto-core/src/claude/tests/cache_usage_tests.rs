use super::*;
use crate::model::Usage;

fn cached_response(
    initial: Value,
    updates: &[Value],
) -> Result<crate::provider::Response, StreamError> {
    let mut body = event(json!({"type":"message_start","message":{
        "id":"cached","role":"assistant","content":[],"usage":initial
    }}));
    for usage in updates {
        body += &event(json!({"type":"message_delta","delta":{},"usage":usage}));
    }
    decode(&(body + &stop("end_turn")))
}

#[test]
fn anchors_the_system_text_and_the_last_message_block_only() {
    let request = Request {
        model: "fixture".into(),
        system_prompt: "stable system".into(),
        tools: vec!["first", "last"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                ..Default::default()
            })
            .collect(),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![Block::text("question")],
            ..Default::default()
        }],
        ..Default::default()
    };
    let encoded = serde_json::to_vec(&protocol::build_request(&request).unwrap()).unwrap();
    assert_eq!(
        encoded,
        serde_json::to_vec(&protocol::build_request(&request).unwrap()).unwrap()
    );
    let body: Value = serde_json::from_slice(&encoded).unwrap();
    let control = json!({"type":"ephemeral"});
    assert!(body.get("cache_control").is_none());
    assert_eq!(
        body["system"][0],
        json!({"type":"text","text":"stable system","cache_control":control})
    );
    assert!(body["tools"][0].get("cache_control").is_none());
    assert!(body["tools"][1].get("cache_control").is_none());
    assert_eq!(
        body["messages"][0]["content"][0],
        json!({"type":"text","text":"question","cache_control":control})
    );
    assert_eq!(
        encoded.len(),
        protocol::serialized_request_size(&request).unwrap()
    );

    let empty =
        serde_json::to_value(protocol::build_request(&Request::default()).unwrap()).unwrap();
    assert!(empty.get("tools").is_none());
    assert!(empty.get("system").is_none());
    assert!(empty.get("cache_control").is_none());
    assert_eq!(empty["messages"], json!([]));
}

#[test]
fn cumulative_cache_usage_counts_each_partition_once_and_preserves_partial_updates() {
    let initial = json!({"input_tokens":7,"output_tokens":1,
        "cache_read_input_tokens":60,"cache_creation_input_tokens":30,
        "cache_creation":{"ephemeral_5m_input_tokens":20,"ephemeral_1h_input_tokens":10}});
    let response = cached_response(initial, &[
        json!({"output_tokens":3,"cache_creation_input_tokens":35,
            "cache_creation":{"ephemeral_1h_input_tokens":15}}),
        json!({"cache_creation_input_tokens":35,"cache_read_input_tokens":60}),
        json!({"cache_creation_input_tokens":null,"cache_read_input_tokens":null,"cache_creation":null}),
    ]).unwrap();
    assert!(response.cached_input_tokens_present);
    assert_eq!(
        response.usage(),
        Some(Usage {
            input_tokens: 102,
            output_tokens: 7,
            cached_input_tokens: 60,
            cache_write_input_tokens: Some(35),
            cache_write_1h_input_tokens: Some(15),
        })
    );
}

#[test]
fn absent_counts_stay_unknown_and_explicit_zero_is_reported() {
    let absent = cached_response(json!({"input_tokens":20}), &[]).unwrap();
    assert!(!absent.cached_input_tokens_present);
    assert_eq!(absent.usage().unwrap().cache_write_input_tokens, None);
    for read in [None, Some(0)] {
        let mut usage = json!({"input_tokens":20,"cache_creation_input_tokens":0});
        if let Some(read) = read {
            usage["cache_read_input_tokens"] = json!(read);
        }
        let response = cached_response(usage, &[]).unwrap();
        assert_eq!(response.cached_input_tokens_present, read.is_some());
        assert_eq!(response.usage().unwrap().cache_write_input_tokens, Some(0));
        assert_eq!(response.usage().unwrap().cache_write_1h_input_tokens, None);
    }
    let nested = cached_response(
        json!({"input_tokens":2,"cache_creation":{
        "ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}}),
        &[],
    )
    .unwrap();
    assert_eq!(nested.usage().unwrap().input_tokens, 9);
    assert_eq!(nested.usage().unwrap().cache_write_input_tokens, Some(7));
}

#[test]
fn rejects_malformed_cache_counts_inconsistent_lifetimes_and_input_overflow() {
    for field in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
        for value in [
            json!(-1),
            json!(1.5),
            json!("1"),
            json!(true),
            json!(18446744073709551615u64),
        ] {
            let mut usage = json!({"input_tokens":2});
            usage[field] = value;
            assert_eq!(
                cached_response(usage, &[]).unwrap_err(),
                StreamError::Decode
            );
        }
    }
    for usage in [
        json!({"input_tokens":i64::MAX,"cache_read_input_tokens":1}),
        json!({"input_tokens":i64::MAX,"cache_creation_input_tokens":1}),
        json!({"cache_creation_input_tokens":2,"cache_creation":{"ephemeral_1h_input_tokens":3}}),
        json!({"cache_creation_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":3}}),
        json!({"cache_creation_input_tokens":3,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":2}}),
        json!({"cache_creation":{"ephemeral_5m_input_tokens":i64::MAX,"ephemeral_1h_input_tokens":1}}),
        json!({"cache_creation":[]}),
        json!({"cache_creation":{"ephemeral_5m_input_tokens":3}}),
        json!({"cache_creation":{"ephemeral_1h_input_tokens":-1}}),
    ] {
        assert_eq!(
            cached_response(usage, &[]).unwrap_err(),
            StreamError::Decode
        );
    }
}
