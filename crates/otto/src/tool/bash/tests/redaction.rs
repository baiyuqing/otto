//! Tests for secret redaction and the collision-safe marker.

use super::*;

#[tokio::test]
async fn split_and_overlapping_secrets_are_redacted_before_the_cap() {
    let (_dir, workspace) = temp_workspace();
    let long = format!("credential-{}", "z".repeat(32));
    let executor = Arc::new(FakeExecutor {
        stdout_chunks: vec![
            long.as_bytes()[..8].to_vec(),
            long.as_bytes()[8..].to_vec(),
            b" | overlap-".to_vec(),
            b"secret-tail | ".to_vec(),
            b"x".repeat(48),
            long.as_bytes().to_vec(),
        ],
        stderr_chunks: vec![
            b"overlap-".to_vec(),
            b"secret-tail | ".to_vec(),
            b"y".repeat(48),
            long.as_bytes().to_vec(),
        ],
        ..FakeExecutor::default()
    });
    let tool = bash(
        &workspace,
        executor,
        &[],
        32,
        &[&long, "overlap-secret", "secret-tail"],
    );
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error);
    for forbidden in [long.as_str(), &long[..32], "overlap-secret", "secret-tail"] {
        assert!(
            !result.content.contains(forbidden),
            "leaked {forbidden:?}: {result:?}"
        );
    }
    assert!(
        result
            .content
            .contains(&format!("stdout:\n{}", tool.redaction_marker))
    );
    assert!(
        result
            .content
            .contains(&format!("stderr:\n{}", tool.redaction_marker))
    );
    assert_eq!(
        result.content.matches("[truncated:").count(),
        2,
        "{result:?}"
    );
}

#[tokio::test]
async fn the_marker_is_one_collision_safe_rune() {
    let (_dir, workspace) = temp_workspace();
    let cases: &[(&str, &[&str], &str, &str)] = &[
        ("TOKEN", &["TOKEN"], "", ""),
        ("*", &["*", "[REDACTED]"], "", ""),
        (
            "leftTOKENright",
            &["TOKEN", "left*right", "left!right", "[REDACTED]"],
            "left",
            "right",
        ),
        ("[REDACTED]", &["[REDACTED]"], "", ""),
    ];
    for (stdout, values, prefix, suffix) in cases {
        let executor = Arc::new(
            FakeExecutor::chunks(&[stdout], &[]).with_status(ExitStatus {
                code: -1,
                signaled: true,
                signal: format!("signal-{}", values[0]),
                ..Default::default()
            }),
        );
        let tool = bash(&workspace, executor, &[], 1024, values);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error, "{result:?}");
        for value in *values {
            assert!(
                !result.content.contains(value),
                "exposed {value:?}: {result:?}"
            );
        }
        let body = stdout_body(&result.content);
        let marker = body
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .unwrap_or_else(|| panic!("body {body:?} lacks {prefix:?}/{suffix:?}"));
        let mut runes = marker.chars();
        let rune = runes.next().expect("the marker is not empty");
        assert!(
            runes.next().is_none(),
            "marker {marker:?} is more than one rune"
        );
        assert!(!rune.is_control(), "marker {marker:?} is a control rune");
        for byte in marker.as_bytes() {
            for value in *values {
                assert!(
                    !value.as_bytes().contains(byte),
                    "marker byte {byte:#x} occurs in {value:?}"
                );
            }
        }
    }
}

fn printable_ascii() -> String {
    (0x20u8..=0x7e).map(char::from).collect()
}

#[tokio::test]
async fn a_multibyte_marker_cap_cannot_synthesize_a_secret() {
    let (_dir, workspace) = temp_workspace();
    let ascii = printable_ascii();
    let values = vec!["TOKEN".to_owned(), "prefix\u{fffd}".to_owned(), ascii];
    let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
    let executor = Arc::new(FakeExecutor::chunks(&["prefixTOKEN"], &[]));
    let tool = bash(&workspace, executor, &[], "prefix".len() + 1, &borrowed);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error);
    for secret in &values {
        assert!(!result.content.contains(secret.as_str()), "{result:?}");
    }
    assert_eq!(stdout_body(&result.content), "pref");
    assert!(
        result.content.contains("[truncated: 2 bytes omitted]"),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_omitted_atomic_marker_cannot_join_bytes_into_another_secret() {
    let (_dir, workspace) = temp_workspace();
    let ascii = printable_ascii();
    let values = vec![
        "TOKEN".to_owned(),
        "prefix\u{fffd}".to_owned(),
        ascii,
        "prefixs".to_owned(),
    ];
    let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
    let executor = Arc::new(FakeExecutor::chunks(&["prefixTOKENsuffix"], &[]));
    let tool = bash(&workspace, executor, &[], "prefix".len() + 1, &borrowed);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error);
    for secret in &values {
        assert!(!result.content.contains(secret.as_str()), "{result:?}");
    }
    assert!(
        result.content.contains("[truncated: 8 bytes omitted]"),
        "{result:?}"
    );
}

#[tokio::test]
async fn invalid_child_bytes_are_normalized_then_redacted() {
    let (_dir, workspace) = temp_workspace();
    let ascii = printable_ascii();
    let values = ["TOKEN".to_owned(), "prefix\u{fffd}".to_owned(), ascii];
    let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
    let executor = Arc::new(FakeExecutor {
        stdout_chunks: vec![b"prefix\xff".to_vec()],
        ..FakeExecutor::default()
    });
    let tool = bash(&workspace, executor, &[], 1024, &borrowed);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error);
    assert!(!result.content.contains("prefix\u{fffd}"), "{result:?}");
}

#[tokio::test]
async fn invalid_redactions_are_canonicalized_separately_from_the_environment() {
    let (_dir, workspace) = temp_workspace();
    let invalid_ff = String::from_utf8_lossy(b"decoded-prefix\xff").into_owned();
    let invalid_overlong = String::from_utf8_lossy(b"overlong-prefix\xc0\xaf").into_owned();
    let environment = vec![format!("RAW_ENDPOINT={invalid_ff}")];
    for max_output in [1usize, 1024] {
        let expected = environment.clone();
        let executor = Arc::new(FakeExecutor::behaving(Box::new(
            move |request, streams, _| {
                let expected = expected.clone();
                Box::pin(async move {
                    assert_eq!(request.env, expected, "the environment was modified");
                    for byte in b"decoded-prefix\xff" {
                        streams.stdout.write_all(&[*byte]).unwrap();
                    }
                    for byte in b"overlong-prefix\xc0\xaf" {
                        streams.stderr.write_all(&[*byte]).unwrap();
                    }
                    (ExitStatus::default(), Ok(()))
                })
            },
        )));
        let tool = BashTool::new(
            &workspace,
            executor,
            "/bin/sh",
            environment.clone(),
            Duration::from_secs(3),
            max_output,
            &[invalid_ff.clone(), invalid_overlong.clone()],
            Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
        )
        .expect("the configuration is valid");
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error, "{result:?}");
        for secret in ["decoded-prefix\u{fffd}", "overlong-prefix\u{fffd}\u{fffd}"] {
            assert!(
                !result.content.contains(secret),
                "cap {max_output}: {result:?}"
            );
        }
        if max_output == 1 {
            assert_eq!(stdout_body(&result.content), "");
            assert!(
                result
                    .content
                    .contains("stdout:\n[truncated: 3 bytes omitted]"),
                "{result:?}"
            );
            assert!(
                result
                    .content
                    .contains("stderr:\n[truncated: 3 bytes omitted]"),
                "{result:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_collision_safe_marker_survives_every_byte_cap() {
    let (_dir, workspace) = temp_workspace();
    let values = vec!["TOKEN".to_owned(), printable_ascii()];
    let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
    let for_cap = |cap: usize| {
        let executor = Arc::new(
            FakeExecutor::chunks(&["TOKEN"], &[]).with_status(ExitStatus {
                code: -1,
                signaled: true,
                signal: "signal-TOKEN".to_owned(),
                ..Default::default()
            }),
        );
        bash(&workspace, executor, &[], cap, &borrowed)
    };

    let full = run(&for_cap(1024), "ignored").await;
    assert!(!full.is_error, "{full:?}");
    let marker = stdout_body(&full.content);
    assert_eq!(marker.chars().count(), 1);
    assert!(
        marker.len() > 1,
        "fallback marker {marker:?} should be multibyte"
    );

    for cap in 1..=marker.len() + 1 {
        let result = run(&for_cap(cap), "ignored").await;
        assert!(!result.is_error, "cap {cap}: {result:?}");
        for value in &values {
            assert!(
                !result.content.contains(value.as_str()),
                "cap {cap}: {result:?}"
            );
        }
        let want = if cap < marker.len() {
            ""
        } else {
            marker.as_str()
        };
        assert_eq!(stdout_body(&result.content), want, "cap {cap}");
        if cap < marker.len() {
            assert!(
                result
                    .content
                    .contains(&format!("[truncated: {} bytes omitted]", marker.len())),
                "cap {cap}: {result:?}"
            );
        }
    }
}

/// Every UTF-8 byte class in one secret, port of `markerByteClassAdversary`.
fn marker_byte_class_adversary() -> String {
    let mut value: Vec<u8> = (0x20u8..=0x7e).collect();
    for continuation in 0x80u8..=0xbf {
        value.extend_from_slice(&[0xc2, continuation]);
    }
    for lead in 0xc3u8..=0xdf {
        value.extend_from_slice(&[lead, 0x80]);
    }
    value.extend_from_slice(&[0xe0, 0xa0, 0x80]);
    for lead in 0xe1u8..=0xec {
        value.extend_from_slice(&[lead, 0x80, 0x80]);
    }
    value.extend_from_slice(&[0xed, 0x80, 0x80]);
    for lead in 0xeeu8..=0xef {
        value.extend_from_slice(&[lead, 0x80, 0x80]);
    }
    value.extend_from_slice(&[0xf0, 0x90, 0x80, 0x80]);
    for lead in 0xf1u8..=0xf3 {
        value.extend_from_slice(&[lead, 0x80, 0x80, 0x80]);
    }
    value.extend_from_slice(&[0xf4, 0x80, 0x80, 0x80]);
    String::from_utf8(value).expect("the adversary is valid UTF-8")
}

#[tokio::test]
async fn every_utf8_byte_class_in_a_secret_still_constructs() {
    let (_dir, workspace) = temp_workspace();
    let secret = marker_byte_class_adversary();
    let executor = Arc::new(FakeExecutor::chunks(&[&secret], &[]));
    let tool = bash(&workspace, executor, &[], 1024, &[&secret]);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error, "{result:?}");
    assert!(!result.content.contains(&secret), "{result:?}");
}

#[tokio::test]
async fn json_string_escapes_in_secrets_are_expanded() {
    let (_dir, workspace) = temp_workspace();
    let cases: &[(&str, &str)] = &[
        (r"less<than", "less<than"),
        (r"greater>than", "greater>than"),
        (r"amp&ersand", "amp&ersand"),
        (r#"quote\"value"#, "quote\"value"),
        (r"back\\slash", r"back\slash"),
        (r"slash\/value", "slash/value"),
        (r"line\nbreak", "line\nbreak"),
        (r"separator value", "separator\u{2028}value"),
        (r"paragraph value", "paragraph\u{2029}value"),
    ];
    let values: Vec<&str> = cases.iter().map(|(raw, _)| *raw).collect();
    let chunks: Vec<String> = cases
        .iter()
        .map(|(_, decoded)| format!("{decoded}\n"))
        .collect();
    let executor = Arc::new(FakeExecutor {
        stdout_chunks: chunks
            .iter()
            .map(|chunk| chunk.as_bytes().to_vec())
            .collect(),
        ..FakeExecutor::default()
    });
    let tool = bash(&workspace, executor, &[], 64 << 10, &values);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error, "{result:?}");
    for (raw_form, decoded) in cases {
        assert!(!result.content.contains(raw_form), "{result:?}");
        assert!(!result.content.contains(decoded), "{result:?}");
    }
}

/// Every non-control scalar value, port of `allNonControlSandboxRunes`.
fn all_non_control_runes() -> String {
    (1u32..=char::MAX as u32)
        .filter_map(char::from_u32)
        .filter(|rune| !rune.is_control())
        .collect()
}

#[tokio::test]
async fn an_exhausted_marker_set_suppresses_all_result_text() {
    let (_dir, workspace) = temp_workspace();
    let marker = otto_core::safetext::dynamic_redaction_marker(&[])
        .expect("the empty set has a shared marker");
    let values = [
        all_non_control_runes(),
        format!("{}z", "a".repeat(1 << 20)),
        marker,
        "X".to_owned(),
        "ab".to_owned(),
    ];
    let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
    let executor = Arc::new(FakeExecutor::chunks(&["a", "X", "b"], &["aX", "b"]));
    let tool = bash(&workspace, executor.clone(), &[], 2 << 20, &borrowed);
    assert!(tool.redaction_marker.is_empty());
    assert!(!tool.dynamic_content);

    let result = run(&tool, "ignored").await;
    assert_eq!(result.content, "");
    assert_eq!(executor.calls(), 1);

    let invalid = tool
        .execute(
            &raw(r#"{"command":123,"X":"X"}"#),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(invalid.content, "");
    assert!(invalid.is_error);
    assert_eq!(executor.calls(), 1);
}

#[tokio::test]
async fn unrepresentable_output_still_runs_the_command_once() {
    let (_dir, workspace) = temp_workspace();
    const HALF: usize = 4 << 10;
    let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, streams, _| {
        Box::pin(async move {
            for _ in 0..HALF {
                streams.stdout.write_all(b"a").unwrap();
            }
            streams.stdout.write_all(b"X").unwrap();
            for _ in 0..HALF {
                streams.stdout.write_all(b"b").unwrap();
            }
            (ExitStatus::default(), Ok(()))
        })
    })));
    let secret = format!("{}z", "a".repeat(1 << 20));
    let tool = bash(&workspace, executor.clone(), &[], 2 * HALF + 1, &[&secret]);
    for _ in 0..3 {
        assert_eq!(run(&tool, "ignored").await.content, "");
    }
    assert_eq!(executor.calls(), 3);
}

#[tokio::test]
async fn an_earlier_fragment_is_held_until_the_later_match_resolves() {
    let (_dir, workspace) = temp_workspace();
    const LONG: &str = "credential-zzSHORT-rest";
    for split in 0..=LONG.len() {
        let executor = Arc::new(FakeExecutor::chunks(&[&LONG[..split], &LONG[split..]], &[]));
        let tool = bash(&workspace, executor, &[], 12, &[LONG, "SHORT"]);
        let result = run(&tool, "ignored").await;
        assert!(!result.is_error, "split {split}: {result:?}");
        assert_eq!(
            stdout_body(&result.content),
            tool.redaction_marker,
            "split {split}"
        );
        assert!(
            !result.content.contains("credential-"),
            "split {split}: {result:?}"
        );
        for value in [LONG, "SHORT"] {
            assert!(!result.content.contains(value), "split {split}: {result:?}");
        }
    }
}

#[tokio::test]
async fn strict_json_errors_are_redacted_and_never_delegate() {
    let (_dir, workspace) = temp_workspace();
    let cases: &[(&str, &str, &str)] = &[
        (
            "private-field",
            r#"{"command":"true","private-field":"private-field"}"#,
            "json: unknown field",
        ),
        ("number", r#"{"command":123}"#, "invalid JSON:"),
        ("command", "{}", "missing required argument:"),
    ];
    for (secret, arguments, want) in cases {
        let executor = Arc::new(FakeExecutor::default());
        let tool = bash(&workspace, executor.clone(), &[], 1024, &[secret]);
        let result = tool
            .execute(&raw(arguments), &CancellationToken::new())
            .await;
        assert!(result.is_error, "{arguments}: {result:?}");
        assert!(result.content.contains(want), "{arguments}: {result:?}");
        assert!(!result.content.contains(secret), "{arguments}: {result:?}");
        assert_eq!(
            outcome(&result).effect_certainty,
            otto_core::model::EffectCertainty::NotStarted
        );
        assert_eq!(executor.calls(), 0, "{arguments}");
    }
}
