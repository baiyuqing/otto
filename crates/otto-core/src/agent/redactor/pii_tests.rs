use super::*;

const EMAIL: &str = "alice@example.com";
const PHONE: &str = "+86 138 0013 8000";
const PAYMENT_CARD: &str = "4111 1111 1111 1111";
const ID_DOCUMENT: &str = "11010519491231002X";

#[test]
fn provider_guard_replaces_supported_pii_with_stable_typed_placeholders() {
    let request = Request {
        system_prompt: format!(
            "contact {EMAIL} or {PHONE}; charge {PAYMENT_CARD}; identity {ID_DOCUMENT}"
        ),
        messages: vec![crate::model::Message {
            role: crate::model::Role::User,
            blocks: vec![crate::model::Block {
                block_type: BlockType::ToolCall,
                text: format!("same {EMAIL}; another bob@example.net"),
                tool_name: "send".into(),
                tool_call_id: "call-1".into(),
                arguments: Some(
                    RawValue::from_string(format!(
                        r#"{{"recipient":"{EMAIL}","phone":"{PHONE}"}}"#
                    ))
                    .expect("valid JSON"),
                ),
                ..crate::model::Block::default()
            }],
            ..crate::model::Message::default()
        }],
        ..Request::default()
    };

    let guarded = Redactor::new(&[])
        .guard_provider_request(&request)
        .expect("PII can be replaced in provider prose");
    let rendered = format!("{guarded:?}");
    for original in [EMAIL, PHONE, PAYMENT_CARD, ID_DOCUMENT, "bob@example.net"] {
        assert!(!rendered.contains(original), "retained {original}");
    }
    for placeholder in [
        "<EMAIL_1>",
        "<EMAIL_2>",
        "<PHONE_1>",
        "<PAYMENT_CARD_1>",
        "<ID_DOCUMENT_1>",
    ] {
        assert!(rendered.contains(placeholder), "missing {placeholder}");
    }
    assert_eq!(rendered.matches("<EMAIL_1>").count(), 2);
    assert_eq!(rendered.matches("<PHONE_1>").count(), 1);
    let arguments = guarded.messages[0].blocks[0]
        .arguments
        .as_ref()
        .expect("arguments")
        .get();
    let parsed: serde_json::Value = serde_json::from_str(arguments).expect("valid JSON");
    assert_eq!(parsed["recipient"], "<EMAIL_1>");
    assert_eq!(parsed["phone"], "<PHONE_1>");
}

#[test]
fn provider_guard_preserves_values_without_high_confidence_evidence() {
    let text = concat!(
        "name Alice; address 1 Main Street; local phone 13800138000; ",
        "invalid email alice@example and alice..smith@example.com; ",
        "invalid card 4111111111111112; ",
        "invalid identity 110105194912310021"
    );
    let request = Request {
        system_prompt: text.into(),
        ..Request::default()
    };
    let guarded = Redactor::new(&[])
        .guard_provider_request(&request)
        .expect("low-confidence text is preserved");
    assert_eq!(guarded.system_prompt, text);
}

#[test]
fn provider_guard_refuses_pii_in_structural_or_opaque_fields() {
    for request in [
        Request {
            model: EMAIL.into(),
            ..Request::default()
        },
        Request {
            tools: vec![crate::model::ToolDefinition {
                name: "send".into(),
                description: "safe".into(),
                parameters: Some(
                    RawValue::from_string(format!(r#"{{"example":"{EMAIL}"}}"#))
                        .expect("valid schema"),
                ),
            }],
            ..Request::default()
        },
        Request {
            messages: vec![crate::model::Message {
                role: crate::model::Role::Assistant,
                blocks: vec![crate::model::Block {
                    block_type: BlockType::ToolCall,
                    tool_name: "send".into(),
                    tool_call_id: "call-1".into(),
                    arguments: Some(
                        RawValue::from_string(format!(r#"{{"{EMAIL}":"value"}}"#))
                            .expect("valid arguments"),
                    ),
                    ..crate::model::Block::default()
                }],
                ..crate::model::Message::default()
            }],
            ..Request::default()
        },
    ] {
        assert!(matches!(
            Redactor::new(&[]).guard_provider_request(&request),
            Err(AgentError::ProviderEgressSecret)
        ));
    }
}

#[test]
fn transient_compaction_sanitization_leaves_pii_for_the_complete_request() {
    let token = ["gh", "p_", "abcdefghijkl", "mnopqrstuvwx"].concat();
    let request = Request {
        system_prompt: format!("email {EMAIL}; TOKEN={token}"),
        ..Request::default()
    };
    let guarded = Redactor::new(&[])
        .guard_provider_request_secrets_only(&request)
        .expect("transient input can be sanitized");
    assert!(guarded.system_prompt.contains(EMAIL));
    assert!(!guarded.system_prompt.contains(&token));
}
