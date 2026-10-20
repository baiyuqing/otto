use super::*;

fn values(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn redact_json(credentials: &[&str], raw: &str) -> String {
    Redactor::new(&values(credentials))
        .redact_json_strings(raw)
        .get()
        .to_owned()
}

/// Every non-control rune, so marker selection has nothing left to pick.
fn all_non_control_runes(excluded: &[char]) -> String {
    let mut value = String::new();
    for candidate in 1u32..=char::MAX as u32 {
        let Some(character) = char::from_u32(candidate) else {
            continue;
        };
        if character.is_control() || excluded.contains(&character) {
            continue;
        }
        value.push(character);
    }
    assert!(
        value.contains(REDACTION_MARKER),
        "fixture omitted the marker"
    );
    value
}

fn candidate() -> String {
    ["gh", "p_", "abcdefghijkl", "mnopqrstuvwx"].concat()
}

#[test]
fn egress_guard_detects_high_confidence_forms_and_ignores_placeholders() {
    let token = candidate();
    assert!(heuristic_secret(&format!("Authorization: Bearer {token}")));
    assert!(heuristic_secret(&format!(
        "Authorization: BeArEr   \t{token}"
    )));
    assert!(heuristic_secret(&format!("private-key = {token}")));
    assert!(heuristic_secret(
        &[
            "-----BEGIN ",
            "PRIVATE KEY-----\nbody\n-----END PRIVATE KEY-----",
        ]
        .concat()
    ));
    assert!(strong_prefixed_token(&token));
    assert!(!strong_prefixed_token("ghp_example"));
    assert!(!heuristic_secret("API_KEY=<redacted>"));
    assert!(!heuristic_secret("TOKEN=your-token"));
    assert!(!heuristic_secret(
        "The authorization requirements use a bearer identity."
    ));
}

#[test]
fn egress_guard_preserves_non_ascii_authorization_value() {
    let authorization = "Authorization: €€€";
    let request = Request {
        system_prompt: authorization.into(),
        ..Request::default()
    };
    let guarded = Redactor::new(&[])
        .guard_provider_request(&request)
        .expect("non-Bearer authorization is benign");
    assert_eq!(guarded.system_prompt, authorization);
}

#[test]
fn egress_guard_preserves_benign_authorization_and_bearer_prose() {
    let prose = "Discuss authorization requirements and bearer identity semantics.";
    let request = Request {
        system_prompt: prose.into(),
        messages: vec![crate::model::Message {
            role: crate::model::Role::User,
            blocks: vec![crate::model::Block::text(prose)],
            ..crate::model::Message::default()
        }],
        ..Request::default()
    };
    let guarded = Redactor::new(&[])
        .guard_provider_request(&request)
        .expect("benign prose");
    assert_eq!(guarded.system_prompt, prose);
    assert_eq!(guarded.messages[0].blocks[0].text, prose);

    let token = candidate();
    let assignment = Request {
        system_prompt: format!("prefix API_KEY={token} suffix"),
        ..Request::default()
    };
    let guarded = Redactor::new(&[])
        .guard_provider_request(&assignment)
        .expect("sensitive assignment");
    assert!(!guarded.system_prompt.contains(&token));
    assert!(guarded.system_prompt.starts_with("prefix API_KEY="));
    assert!(guarded.system_prompt.ends_with(" suffix"));
}

#[test]
fn egress_guard_redacts_only_the_assigned_occurrence() {
    let candidate = "abcdefgh";
    let request = Request {
        system_prompt: format!("API_KEY={candidate}\nThe unrelated identifier is {candidate}"),
        ..Request::default()
    };
    let guarded = Redactor::new(&[])
        .guard_provider_request(&request)
        .expect("guarded request");
    assert!(
        !guarded
            .system_prompt
            .starts_with(&format!("API_KEY={candidate}"))
    );
    assert!(
        guarded
            .system_prompt
            .ends_with(&format!("identifier is {candidate}"))
    );
}

#[test]
fn egress_guard_redacts_json_strings_and_preserves_valid_json() {
    let token = candidate();
    let request = Request {
        system_prompt: format!("API_KEY={token}"),
        messages: vec![crate::model::Message {
            role: crate::model::Role::Assistant,
            blocks: vec![crate::model::Block {
                block_type: BlockType::ToolCall,
                tool_name: "echo".into(),
                tool_call_id: "call-1".into(),
                arguments: Some(
                    RawValue::from_string(format!(r#"{{"{token}":{{"api_key":"{token}"}}}}"#))
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
        .expect("guarded request");
    assert!(!guarded.system_prompt.contains(&token));
    let arguments = guarded.messages[0].blocks[0]
        .arguments
        .as_ref()
        .expect("arguments")
        .get();
    assert!(!arguments.contains(&token));
    assert!(serde_json::from_str::<serde_json::Value>(arguments).is_ok());
    assert!(
        request.system_prompt.contains(&token),
        "input clone unchanged"
    );
}

#[test]
fn egress_guard_associates_json_sensitive_keys_with_their_values() {
    let candidate = "abcdefgh";
    let request = Request {
        messages: vec![crate::model::Message {
            role: crate::model::Role::Assistant,
            blocks: vec![crate::model::Block {
                block_type: BlockType::ToolCall,
                arguments: Some(
                    RawValue::from_string(format!(
                        r#"{{"token":"redacted","api_key":"{candidate}","query":"{candidate}"}}"#
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
        .expect("guarded request");
    let arguments = guarded.messages[0].blocks[0]
        .arguments
        .as_ref()
        .expect("arguments")
        .get();
    let parsed: serde_json::Value = serde_json::from_str(arguments).expect("valid JSON");
    assert_eq!(parsed["token"], "redacted");
    assert_eq!(parsed["query"], candidate);
    assert_ne!(parsed["api_key"], candidate);
}

#[test]
fn egress_guard_removes_known_exact_secret_and_blocks_incomplete_redactor() {
    let secret = "known-exact-provider-secret";
    let request = Request {
        system_prompt: format!("prefix API_KEY={secret} suffix"),
        ..Request::default()
    };
    let guarded = Redactor::new(&[secret.into()])
        .guard_provider_request(&request)
        .expect("known secret is removable from prose");
    assert!(!guarded.system_prompt.contains(secret));
    assert!(guarded.system_prompt.starts_with("prefix API_KEY="));
    assert!(guarded.system_prompt.ends_with(" suffix"));
    let incomplete = Redactor {
        values: Vec::new(),
        marker: String::new(),
        complete: false,
    };
    assert!(matches!(
        incomplete.guard_provider_request(&Request::default()),
        Err(AgentError::ProviderEgressSecret)
    ));
}

#[test]
fn egress_guard_refuses_protected_image_and_tool_definition_fields() {
    let token = candidate();
    let image_request = Request {
        messages: vec![crate::model::Message {
            role: crate::model::Role::User,
            blocks: vec![crate::model::Block {
                block_type: BlockType::Image,
                data: token.clone(),
                mime_type: "image/png".into(),
                ..crate::model::Block::default()
            }],
            ..crate::model::Message::default()
        }],
        ..Request::default()
    };
    assert!(matches!(
        Redactor::new(&[]).guard_provider_request(&image_request),
        Err(AgentError::ProviderEgressSecret)
    ));
    let tool_request = Request {
        tools: vec![crate::model::ToolDefinition {
            name: "echo".into(),
            description: format!("TOKEN={token}"),
            parameters: None,
        }],
        ..Request::default()
    };
    assert!(matches!(
        Redactor::new(&[]).guard_provider_request(&tool_request),
        Err(AgentError::ProviderEgressSecret)
    ));
    let cache_request = Request {
        cache_key: token.clone(),
        ..Request::default()
    };
    assert!(matches!(
        Redactor::new(&[]).guard_provider_request(&cache_request),
        Err(AgentError::ProviderEgressSecret)
    ));
    let schema_request = Request {
        tools: vec![crate::model::ToolDefinition {
            name: "echo".into(),
            description: "safe".into(),
            parameters: Some(
                RawValue::from_string(format!(r#"{{"api_key":"{token}"}}"#)).expect("valid schema"),
            ),
        }],
        ..Request::default()
    };
    assert!(matches!(
        Redactor::new(&[]).guard_provider_request(&schema_request),
        Err(AgentError::ProviderEgressSecret)
    ));
}

#[test]
fn expands_json_string_escape_forms_before_marker_selection() {
    let forms = [
        (r"\u003c", "<"),
        (r"\u003C", "<"),
        (r"\u003e", ">"),
        (r"\u0026", "&"),
        (r#"\""#, "\""),
        (r"\\", "\\"),
        (r"\/", "/"),
        (r"\n", "\n"),
        (r"\u2028", "\u{2028}"),
        (r"\u2029", "\u{2029}"),
    ];
    let mut excluded = Vec::new();
    let mut configured = Vec::new();
    for (raw, decoded) in forms {
        excluded.extend(decoded.chars());
        configured.push(raw.to_owned());
    }
    configured.push(all_non_control_runes(&excluded));
    let redactor = Redactor::new(&configured);

    for (raw, decoded) in forms {
        for input in [raw, decoded] {
            let redacted = redactor.redact_string(input);
            assert!(
                !redacted.contains(raw) && !redacted.contains(decoded),
                "redact_string({input:?}) retained a configured form: {redacted:?}"
            );
            let encoded = serde_json::to_string(&redacted).expect("encodes");
            assert!(
                !encoded.contains(raw),
                "JSON encoding recreated the configured form {raw:?}: {encoded}"
            );
        }
    }
}

#[test]
fn incomplete_redactor_suppresses_every_dynamic_boundary() {
    let redactor = Redactor::with_completeness(&values(&["known-secret"]), false);
    assert_eq!(redactor.redact_string("unknown-attacker-content"), "");
    assert_eq!(
        redactor
            .redact_json_strings(r#"{"unknown":"content"}"#)
            .get(),
        "null"
    );
    let boundary = redactor.redact_error(AgentError::Provider(ProviderError::Cancelled));
    assert!(matches!(
        boundary,
        AgentError::Provider(ProviderError::Cancelled)
    ));
    let mut stream = redactor.new_stream();
    assert_eq!(stream.write("unknown-"), "");
    assert_eq!(stream.write("attacker-content"), "");
    assert_eq!(stream.flush(), "");
}

#[test]
fn incomplete_redactor_replaces_every_other_error_with_an_empty_message() {
    let redactor = Redactor::with_completeness(&values(&["known-secret"]), false);
    let redacted = redactor.redact_error(AgentError::Other("known-secret leaked".into()));
    assert_eq!(redacted.to_string(), "");
    assert!(matches!(redacted, AgentError::Redacted { .. }));
}

#[test]
fn preserves_invalid_compaction_summary_identity() {
    let redactor = Redactor::new(&values(&["secret"]));
    let redacted = redactor.redact_error(AgentError::InvalidCompactionSummary(
        "contains secret".into(),
    ));
    assert!(redacted.is_invalid_compaction_summary());
    assert!(!redacted.to_string().contains("secret"));
}

#[test]
fn preserves_cancellation_identity() {
    let redactor = Redactor::new(&values(&["cancelled"]));
    let redacted = redactor.redact_error(AgentError::Provider(ProviderError::Cancelled));
    assert!(redacted.is_cancelled());
    assert!(!redacted.to_string().contains("cancelled"));
}

#[test]
fn returns_the_original_error_when_nothing_needed_redacting() {
    let redactor = Redactor::new(&values(&["secret"]));
    let redacted = redactor.redact_error(AgentError::EmptyUserText);
    assert!(matches!(redacted, AgentError::EmptyUserText));
}

#[test]
fn uses_stable_single_rune_preferred_markers() {
    let first = Redactor::new(&[]);
    assert_eq!(first.marker, REDACTION_MARKER);
    assert_eq!(first.marker.chars().count(), 1);
    let second = Redactor::new(&values(&[REDACTION_MARKER]));
    assert_eq!(second.marker, "\u{E001}");
    assert_eq!(second.marker.chars().count(), 1);
}

#[test]
fn never_uses_a_replacement_that_contains_the_credential() {
    for credential in ["[REDACTED]", "REDACTED", "[", "界"] {
        let redactor = Redactor::new(&values(&[credential]));
        let redacted = redactor.redact_string(&format!("before {credential} after"));
        assert!(
            !redacted.contains(credential),
            "redact_string() still contains {credential:?}: {redacted:?}"
        );
    }
}

#[test]
fn replacement_cannot_synthesize_another_credential() {
    const SOURCE: &str = "source-secret";
    const SYNTHESIZED: &str = "a[";
    let redactor = Redactor::new(&values(&[SOURCE, SYNTHESIZED]));

    let redacted = redactor.redact_string(&format!("a{SOURCE}"));
    for credential in [SOURCE, SYNTHESIZED] {
        assert!(
            !redacted.contains(credential),
            "redact_string() synthesized {credential:?}: {redacted:?}"
        );
    }

    let mut stream = redactor.new_stream();
    let mut streamed = stream.write("a");
    for character in SOURCE.chars() {
        streamed.push_str(&stream.write(&character.to_string()));
    }
    streamed.push_str(&stream.flush());
    for credential in [SOURCE, SYNTHESIZED] {
        assert!(
            !streamed.contains(credential),
            "stream synthesized {credential:?}: {streamed:?}"
        );
    }
}

#[test]
fn stream_emits_the_same_text_as_a_single_redaction() {
    let redactor = Redactor::new(&values(&["secret", "token-value"]));
    let input = "a token-value and a secret and a sec";
    let mut stream = redactor.new_stream();
    let mut streamed = String::new();
    for character in input.chars() {
        streamed.push_str(&stream.write(&character.to_string()));
    }
    streamed.push_str(&stream.flush());
    assert_eq!(streamed, redactor.redact_string(input));
}

#[test]
fn exhaustive_marker_fallback_suppresses_dynamic_content() {
    let long_prefix = "a".repeat(1 << 20) + "z";
    let redactor = Redactor::new(&[
        all_non_control_runes(&[]),
        long_prefix,
        "X".to_owned(),
        "ab".to_owned(),
    ]);
    assert_eq!(redactor.marker, "");
    assert!(!redactor.allows_dynamic_content());

    let pathological = "a".repeat(50_000) + "X" + &"b".repeat(50_000);
    assert_eq!(redactor.redact_string(&pathological), "");

    let stream_input = "a".repeat(8 << 10) + "X" + &"b".repeat(8 << 10);
    let mut stream = redactor.new_stream();
    for index in 0..stream_input.len() {
        assert_eq!(stream.write(&stream_input[index..index + 1]), "");
    }
    assert_eq!(stream.flush(), "");
}

#[test]
fn redacts_root_and_nested_json_strings() {
    const CREDENTIAL: &str = "json-root-secret";
    for raw in [
        r#""json-root-secret""#,
        r#"{"nested":["json-root-secret"]}"#,
    ] {
        let got = redact_json(&[CREDENTIAL], raw);
        assert!(
            !got.contains(CREDENTIAL),
            "redact_json_strings({raw}) = {got}"
        );
        serde_json::from_str::<serde_json::Value>(&got).expect("valid JSON");
    }
}

#[test]
fn redacts_json_keys_without_losing_number_fidelity() {
    const CREDENTIAL: &str = "resolved-secret";
    let raw = r#"{"resolved-secret":9007199254740993123456789,"nested":{"prefix-resolved-secret-suffix":1.2300e+45}}"#;
    let got = redact_json(&[CREDENTIAL], raw);
    assert!(!got.contains(CREDENTIAL), "leaked the key: {got}");
    for number in ["9007199254740993123456789", "1.2300e+45"] {
        assert!(got.contains(number), "lost {number} fidelity: {got}");
    }
}

#[test]
fn json_key_redaction_is_safe_and_deterministic() {
    let cases: [(&str, Vec<String>, String); 3] = [
        (
            "credential equals default marker",
            values(&[REDACTION_MARKER]),
            format!(r#"{{"{REDACTION_MARKER}":"first","\\uE001":"second"}}"#),
        ),
        (
            "overlapping credentials",
            values(&["secret", "resolved-secret"]),
            r#"{"resolved-secret":{"secret":"value"}}"#.to_owned(),
        ),
        (
            "root and nested collisions",
            values(&["secret"]),
            format!(
                r#"{{"secret":"first","{REDACTION_MARKER}":"second","nested":{{"secret":1,"{REDACTION_MARKER}":2}}}}"#
            ),
        ),
    ];
    for (name, credentials, raw) in cases {
        let redactor = Redactor::new(&credentials);
        let mut first = String::new();
        for iteration in 0..100 {
            let got = redactor.redact_json_strings(&raw).get().to_owned();
            serde_json::from_str::<serde_json::Value>(&got)
                .unwrap_or_else(|_| panic!("{name}: invalid JSON {got}"));
            for credential in &credentials {
                assert!(!got.contains(credential.as_str()), "{name}: leaked {got}");
            }
            if iteration == 0 {
                first = got;
            } else {
                assert_eq!(got, first, "{name}: nondeterministic output");
            }
        }
        if name == "root and nested collisions" {
            assert_eq!(
                first,
                format!(r#"{{"nested":{{"{REDACTION_MARKER}":null}},"{REDACTION_MARKER}":null}}"#)
            );
        }
    }
}

#[test]
fn json_duplicate_and_normalized_keys_fail_closed() {
    let cases = [
        (
            "exact duplicate",
            r#"{"safe":"first","safe":"attacker"}"#.to_owned(),
            r#"{"safe":null}"#.to_owned(),
        ),
        (
            "escape alias",
            r#"{"a":"first","\u0061":"attacker"}"#.to_owned(),
            r#"{"a":null}"#.to_owned(),
        ),
        (
            "unpaired surrogate variants",
            r#"{"secret-\ud800":"first","secret-\ud801":"attacker"}"#.to_owned(),
            format!(r#"{{"{REDACTION_MARKER}-�":null}}"#),
        ),
        (
            "nested array and post-redaction collision",
            format!(
                r#"{{"items":[{{"secret":"first","{REDACTION_MARKER}":"attacker"}},{{"nested":{{"a":"first","\u0061":"attacker"}}}}]}}"#
            ),
            format!(r#"{{"items":[{{"{REDACTION_MARKER}":null}},{{"nested":{{"a":null}}}}]}}"#),
        ),
    ];
    for (name, raw, want) in cases {
        let got = redact_json(&["secret"], &raw);
        assert_eq!(got, want, "{name}");
        assert!(
            !got.contains("attacker") && !got.contains("secret"),
            "{name}: retained colliding semantics: {got}"
        );
    }
}

#[test]
fn json_invalid_input_and_depth_fail_closed() {
    let too_deep = "[".repeat(10_001) + r#""secret""# + &"]".repeat(10_001);
    for raw in [
        r#"{"secret":"#.to_owned(),
        r#""unterminated"#.to_owned(),
        r#"{"a":1} {"b":2}"#.to_owned(),
        too_deep,
    ] {
        assert_eq!(redact_json(&["secret"], &raw), "null", "input {raw:.32}");
    }
}

#[test]
fn json_maximum_supported_depth_does_not_panic() {
    let raw = "[".repeat(MAXIMUM_JSON_DEPTH) + r#""secret""# + &"]".repeat(MAXIMUM_JSON_DEPTH);
    let got = redact_json(&["secret"], &raw);
    assert!(!got.contains("secret"));
    assert!(got.starts_with("[["));
}

#[test]
fn overlapping_json_key_secrets_do_not_depend_on_configuration_order() {
    let raw = r#"{"abc":"value"}"#;
    assert_eq!(
        redact_json(&["ab", "bc"], raw),
        redact_json(&["bc", "ab"], raw)
    );
}

#[test]
fn json_without_configured_secrets_passes_through() {
    let redactor = Redactor::new(&[]);
    assert_eq!(
        redactor.redact_json_strings(r#"{"path":"file.go"}"#).get(),
        r#"{"path":"file.go"}"#
    );
}

#[test]
fn escapes_html_characters_the_way_encoding_json_does() {
    assert_eq!(
        redact_json(&["secret"], r#"{"html":"<a>&b"}"#),
        r#"{"html":"\u003ca\u003e\u0026b"}"#
    );
}
