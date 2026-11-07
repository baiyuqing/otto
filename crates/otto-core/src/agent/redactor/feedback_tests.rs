use super::*;
use crate::model::{Block, Message, Role};

fn candidate() -> String {
    ["gh", "p_", "abcdefghijkl", "mnopqrstuvwx"].concat()
}

fn tool_request(text: &str) -> Request {
    Request {
        messages: vec![Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                text: text.into(),
                tool_name: "read".into(),
                tool_call_id: "call-1".into(),
                ..Block::default()
            }],
            ..Message::default()
        }],
        ..Request::default()
    }
}

#[test]
fn feedback_distinguishes_unchanged_partial_and_fully_withheld_tool_results() {
    let candidate = candidate();
    let redactor = Redactor::new(std::slice::from_ref(&candidate));
    for (text, status) in [
        ("".to_string(), None),
        ("No matches.".into(), None),
        (
            format!("safe prefix {candidate} safe suffix"),
            Some("partial"),
        ),
        (candidate.clone(), Some("withheld")),
        (REDACTION_MARKER.into(), Some("withheld")),
        ("alice@example.com".into(), Some("withheld")),
        ("<EMAIL_1>".into(), Some("withheld")),
    ] {
        let request = tool_request(&text);
        let guarded = redactor.guard_provider_request(&request).unwrap();
        let output = &guarded.messages[0].blocks[0].text;
        assert!(!output.contains(&candidate));
        assert!(!output.contains("alice@example.com"));
        assert_eq!(
            request.messages[0].blocks[0].text, text,
            "original result unchanged"
        );
        if let Some(status) = status {
            assert!(output.contains(&format!("status=\"{status}\"")), "{output}");
            assert!(output.contains("not literal source text or empty results"));
        } else {
            assert_eq!(output, &text);
        }
    }
}

#[test]
fn feedback_reports_credential_and_personal_data_detection_without_source_values() {
    let redactor = Redactor::new(&[]);
    let candidate = candidate();
    let output = redactor
        .guard_provider_request(&tool_request(&format!("api_key={candidate}")))
        .unwrap();
    let text = &output.messages[0].blocks[0].text;
    assert!(text.contains("reason=\"credential_detection\""));
    assert!(!text.contains(&candidate));
    let output = redactor
        .guard_provider_request(&tool_request("contact: alice@example.com"))
        .unwrap();
    assert!(
        output.messages[0].blocks[0]
            .text
            .contains("reason=\"personal_data\"")
    );
    let output = redactor
        .guard_provider_request(&tool_request(&format!(
            "api_key={candidate}; alice@example.com"
        )))
        .unwrap();
    assert!(
        output.messages[0].blocks[0]
            .text
            .contains("reason=\"sensitive_content\"")
    );
}

#[test]
fn fixed_feedback_cannot_reintroduce_an_exact_secret_or_override_fail_closed_behavior() {
    let redactor = Redactor::new(&["redaction".into()]);
    let output = redactor
        .guard_provider_request(&tool_request("redaction"))
        .unwrap();
    assert!(!output.messages[0].blocks[0].text.contains("redaction"));
    let incomplete = Redactor::with_completeness(&[], false);
    assert!(matches!(
        incomplete.guard_provider_request(&tool_request("anything")),
        Err(AgentError::ProviderEgressSecret)
    ));
}

#[test]
fn nonliteral_source_expressions_and_public_environment_names_remain_readable() {
    let redactor = Redactor::new(&[]);
    for text in [
        "let mut api_key = String::new();",
        "api_key = resolve_api_key(env, &api_key_env);",
        "let password = credentials.password();",
        "let api_key = String::default();",
        "api_key = fetch_key(&account.key)?;",
        "api_key = \"ANTHROPIC_API_KEY\";",
        "api_key = \"OTTO_API_KEY\";",
        "api_key = \"OPENAI_API_KEY\";",
    ] {
        assert!(!heuristic_secret(text), "source was misidentified: {text}");
        assert_eq!(redactor.redact_record_text(text), text);
        let guarded = redactor
            .guard_provider_request(&tool_request(text))
            .unwrap();
        assert_eq!(guarded.messages[0].blocks[0].text, text);
    }
}

#[test]
fn source_exemptions_do_not_apply_to_literals_quoted_expressions_or_registered_values() {
    let candidate = candidate();
    for text in [
        format!("let api_key = \"{candidate}\";"),
        format!("password={candidate}"),
        "api_key = opaquevalue123;".into(),
        "password=ANTHROPIC_API_KEY".into(),
        "token=OPENAI_API_KEY".into(),
        "client_secret=OTTO_API_KEY".into(),
        "api_key = \"String::new()\";".into(),
        "{\"api_key\":\"String::new()\"}".into(),
        format!("let api_key = load(\"{candidate}\");"),
        format!("let api_key = load({candidate});"),
    ] {
        let redactor = Redactor::new(&[]);
        assert!(heuristic_secret(&text), "missed credential context");
        let recorded = redactor.redact_record_text(&text);
        assert_ne!(recorded, text);
        for literal in [
            candidate.as_str(),
            "opaquevalue123",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OTTO_API_KEY",
            "String::new()",
        ] {
            if text.contains(literal) {
                assert!(!recorded.contains(literal), "credential remained");
            }
        }
    }
    for value in [
        "ANTHROPIC_API_KEY",
        "OTTO_API_KEY",
        "String::new()",
        "resolve_api_key(env, &api_key_env)",
    ] {
        let redactor = Redactor::new(&[value.into()]);
        let text = format!("let api_key = {value};");
        let guarded = redactor
            .guard_provider_request(&tool_request(&text))
            .unwrap();
        assert!(!guarded.messages[0].blocks[0].text.contains(value));
        assert!(!redactor.redact_record_text(&text).contains(value));
    }
}

#[test]
fn operational_placeholder_validation_handles_private_and_personal_json_escapes_only() {
    use crate::tool::redaction::{ARGUMENT_ERROR, argument_error};
    for name in ["grep", "find", "read", "ls", "edit", "write", "bash"] {
        for arguments in [
            r#"{"pattern":"\ue000"}"#,
            r#"{"path":"\uE03f/file"}"#,
            r#"{"text":"<EMAIL_1>"}"#,
            r#"{"text":"\u003cPHONE_2\u003e"}"#,
        ] {
            assert_eq!(argument_error(name, arguments), Some(ARGUMENT_ERROR));
        }
        assert_eq!(
            argument_error(name, r#"{"text":"api_key = String::new();"}"#),
            None
        );
    }
    assert_eq!(
        argument_error("request_approval", r#"{"text":"<EMAIL_1>"}"#),
        None
    );
}

#[test]
fn uninspectable_operational_arguments_fail_closed_without_echoing_input() {
    let arguments = r#"{"number":1e400,"pattern":"\ue000"}"#;
    let error = crate::tool::redaction::argument_error("grep", arguments).unwrap();
    assert!(error.starts_with("invalid_arguments:"));
    assert!(!error.contains(arguments));
}
