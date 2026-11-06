use super::*;
use crate::session::pi::PiHeader;

/// The seven checked-in Pi v3 fixtures, embedded so the decoder tests run on
/// every target including wasm32.
pub(crate) const FIXTURES: &[(&str, &[u8])] = &[
    (
        "compacted.jsonl",
        include_bytes!("../../../../../testdata/session/pi-v3/compacted.jsonl"),
    ),
    (
        "compaction-dual-form.jsonl",
        include_bytes!("../../../../../testdata/session/pi-v3/compaction-dual-form.jsonl"),
    ),
    (
        "compaction-retained-tail-only.jsonl",
        include_bytes!("../../../../../testdata/session/pi-v3/compaction-retained-tail-only.jsonl"),
    ),
    (
        "linear.jsonl",
        include_bytes!("../../../../../testdata/session/pi-v3/linear.jsonl"),
    ),
    (
        "memory-recall.jsonl",
        include_bytes!("../../../../../testdata/session/pi-v3/memory-recall.jsonl"),
    ),
    (
        "tree.jsonl",
        include_bytes!("../../../../../testdata/session/pi-v3/tree.jsonl"),
    ),
    (
        "unknown-entry.jsonl",
        include_bytes!("../../../../../testdata/session/pi-v3/unknown-entry.jsonl"),
    ),
];

pub(crate) fn fixture(name: &str) -> &'static [u8] {
    FIXTURES
        .iter()
        .find(|(fixture_name, _)| *fixture_name == name)
        .map(|(_, data)| *data)
        .unwrap_or_else(|| panic!("unknown fixture {name}"))
}

pub(crate) fn read_pi_fixture(name: &str) -> PiFile {
    decode_pi_file(fixture(name)).expect("fixture decodes")
}

macro_rules! test {
        ($name:ident, $body:block) => {
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
            #[cfg_attr(not(target_arch = "wasm32"), test)]
            fn $name() $body
        };
    }

test!(decode_pi_v3_linear_fixture, {
    let decoded = read_pi_fixture("linear.jsonl");
    assert_eq!(decoded.header.type_name, "session");
    assert_eq!(decoded.header.version, 3);
    assert_eq!(decoded.header.cwd, "/workspace");
    assert_eq!(decoded.entries.len(), 4);
    assert!(decoded.entries[0].parent_id.is_none());

    let assistant = decoded.entries[1]
        .message
        .as_ref()
        .expect("assistant message");
    assert_eq!(assistant.role, "assistant");
    assert_eq!(assistant.content_blocks.len(), 2);
    assert_eq!(assistant.content_blocks[1].type_name, "toolCall");

    let result = decoded.entries[2].message.as_ref().expect("tool result");
    assert_eq!(result.role, "toolResult");
    assert_eq!(result.tool_call_id, "call-1");
    assert_eq!(result.is_error, Some(false));
});

test!(decode_pi_v3_tree_fixture_uses_exact_entry_shapes, {
    let decoded = read_pi_fixture("tree.jsonl");
    assert_eq!(
        decoded.header.parent_session.as_deref(),
        Some("/workspace/parent.jsonl")
    );

    let seen = |type_name: &str| -> PiEntry {
        decoded
            .entries
            .iter()
            .rev()
            .find(|entry| entry.type_name == type_name)
            .unwrap_or_else(|| panic!("no {type_name} entry"))
            .clone()
    };
    assert_eq!(
        seen("custom").custom.expect("custom payload").custom_type,
        "otto.runtime"
    );
    assert_eq!(
        seen("model_change")
            .model_change
            .expect("model_change payload")
            .model_id,
        "test-model-2"
    );
    assert!(
        seen("thinking_level_change")
            .thinking_level_change
            .is_some()
    );
    assert_eq!(
        seen("branch_summary")
            .branch_summary
            .expect("branch_summary payload")
            .from_id,
        "a0000003"
    );
    assert_eq!(
        seen("label").label.expect("label payload").target_id,
        "a0000002"
    );
    assert!(
        seen("session_info")
            .session_info
            .expect("session_info payload")
            .name
            .is_some()
    );
});

test!(decode_pi_v3_compaction_fixture, {
    let decoded = read_pi_fixture("compacted.jsonl");
    let legacy = decoded.entries[2]
        .compaction
        .as_ref()
        .expect("legacy compaction");
    assert_eq!(legacy.first_kept_entry_id.as_deref(), Some("c0000002"));

    let checkpoint = decoded.entries[3].compaction.as_ref().expect("checkpoint");
    assert_eq!(checkpoint.summary, "summary");
    assert_eq!(
        checkpoint
            .retained_tail
            .as_deref()
            .unwrap_or_default()
            .len(),
        1
    );
    assert_eq!(
        checkpoint.retained_tail.as_ref().expect("tail")[0].role,
        "user"
    );
    let usage = checkpoint.usage.as_ref().expect("checkpoint usage");
    assert_eq!((usage.input, usage.output), (12, 4));
    assert_eq!(checkpoint.from_hook, Some(false));
});

test!(decode_pi_v3_compaction_boundary_fixtures, {
    let retained = read_pi_fixture("compaction-retained-tail-only.jsonl");
    let checkpoint = retained
        .entries
        .last()
        .expect("entries")
        .compaction
        .as_ref()
        .expect("checkpoint");
    assert!(checkpoint.first_kept_entry_id.is_none());
    assert_eq!(
        checkpoint
            .retained_tail
            .as_deref()
            .unwrap_or_default()
            .len(),
        1
    );
    assert_eq!(
        checkpoint.retained_tail.as_ref().expect("tail")[0].role,
        "user"
    );
    let details = checkpoint.details.as_ref().expect("details").get();
    assert!(
        details.contains(r#""readFiles":"malformed""#),
        "malformed external details were not preserved: {details}"
    );

    let dual = read_pi_fixture("compaction-dual-form.jsonl");
    let checkpoint = dual.entries[3].compaction.as_ref().expect("checkpoint");
    assert_eq!(checkpoint.first_kept_entry_id.as_deref(), Some("e0000003"));
    assert_eq!(
        checkpoint
            .retained_tail
            .as_deref()
            .unwrap_or_default()
            .len(),
        1
    );
});

test!(decode_pi_v3_preserves_unknown_entry_raw_json, {
    let decoded = read_pi_fixture("unknown-entry.jsonl");
    let entry = &decoded.entries[1];
    assert_eq!(entry.type_name, "future_entry");
    let raw = String::from_utf8(entry.raw.clone()).expect("utf-8");
    assert!(raw.contains(r#""futureField""#), "raw: {raw}");
    let encoded = encode_pi_record(PiRecord::Entry(entry)).expect("encode");
    assert_eq!(encoded, entry.raw, "encoded unknown entry changed raw JSON");
});

test!(
    decode_pi_v3_preserves_unknown_fields_on_supported_objects,
    {
        let decoded = read_pi_fixture("unknown-entry.jsonl");
        let header_raw = String::from_utf8(decoded.header.raw.clone()).expect("utf-8");
        assert!(
            header_raw.contains(r#""futureHeaderField""#),
            "{header_raw}"
        );

        for index in [0usize, 2] {
            let entry = &decoded.entries[index];
            let raw = String::from_utf8(entry.raw.clone()).expect("utf-8");
            assert!(raw.contains("future"), "entry {index} raw: {raw}");
            let encoded = encode_pi_record(PiRecord::Entry(entry)).expect("encode");
            assert_eq!(encoded, entry.raw, "entry {index} changed raw JSON");
        }

        let encoded_header =
            encode_pi_record(PiRecord::Header(&decoded.header)).expect("encode header");
        assert_eq!(encoded_header, decoded.header.raw);
    }
);

test!(decode_pi_v3_tolerates_additional_nested_fields, {
    const INPUT: &str = concat!(
        r#"{"type":"message","id":"e0000001","parentId":null,"timestamp":"2026-08-27T12:00:00Z","#,
        r#""message":{"role":"assistant","content":[{"type":"text","text":"ok","#,
        r#""futureText":true}],"api":"openai-completions","provider":"openai-compatible","#,
        r#""model":"test-model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"#,
        r#""totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"#,
        r#""futureCost":0},"futureUsage":0},"stopReason":"stop","timestamp":1787832000000,"#,
        r#""futureMessage":true},"futureEntry":true}"#,
    );
    let entry = decode_pi_entry(INPUT.as_bytes()).expect("decode");
    assert!(entry.message.is_some());
    assert_eq!(entry.raw, INPUT.as_bytes());
});

test!(decode_pi_v3_accepts_final_record_without_lf, {
    let contents = fixture("linear.jsonl");
    let trimmed = contents.strip_suffix(b"\n").expect("fixture ends with LF");
    let decoded = decode_pi_file(trimmed).expect("decode");
    assert_eq!(decoded.entries.len(), 4);
});

test!(decode_pi_v3_rejects_malformed_known_shapes, {
    const BASE: &str = r#""id":"e0000001","parentId":null,"timestamp":"2026-08-27T12:00:00Z""#;
    let usage = r#""usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}"#;
    let cases: Vec<(&str, String)> = vec![
        (
            "missing base parentId",
            r#"{"type":"future_entry","id":"e0000001","timestamp":"2026-08-27T12:00:00Z"}"#.into(),
        ),
        (
            "wrong base id",
            r#"{"type":"future_entry","id":1,"parentId":null,"timestamp":"2026-08-27T12:00:00Z"}"#
                .into(),
        ),
        (
            "custom type",
            format!(r#"{{"type":"custom",{BASE},"customType":1}}"#),
        ),
        (
            "model id",
            format!(r#"{{"type":"model_change",{BASE},"provider":"openai-compatible"}}"#),
        ),
        (
            "thinking level",
            format!(r#"{{"type":"thinking_level_change",{BASE},"thinkingLevel":false}}"#),
        ),
        (
            "message role",
            format!(r#"{{"type":"message",{BASE},"message":{{"role":1,"timestamp":1}}}}"#),
        ),
        (
            "unknown message role",
            format!(
                r#"{{"type":"message",{BASE},"message":{{"role":"futureRole","timestamp":1}}}}"#
            ),
        ),
        (
            "user content",
            format!(
                r#"{{"type":"message",{BASE},"message":{{"role":"user","content":42,"timestamp":1}}}}"#
            ),
        ),
        (
            "unknown content type",
            format!(
                r#"{{"type":"message",{BASE},"message":{{"role":"user","content":[{{"type":"futureBlock"}}],"timestamp":1}}}}"#
            ),
        ),
        (
            "text block",
            format!(
                r#"{{"type":"message",{BASE},"message":{{"role":"user","content":[{{"type":"text","text":false}}],"timestamp":1}}}}"#
            ),
        ),
        (
            "text signature",
            format!(
                r#"{{"type":"message",{BASE},"message":{{"role":"user","content":[{{"type":"text","text":"ok","textSignature":1}}],"timestamp":1}}}}"#
            ),
        ),
        (
            "tool arguments",
            format!(
                concat!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","#,
                    r#""content":[{{"type":"toolCall","id":"call-1","name":"read","#,
                    r#""arguments":[]}}],"api":"openai-completions","#,
                    r#""provider":"openai-compatible","model":"test",{usage},"#,
                    r#""stopReason":"toolUse","timestamp":1}}}}"#,
                ),
                BASE = BASE,
                usage = usage,
            ),
        ),
        (
            "tool namespace",
            format!(
                concat!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","#,
                    r#""content":[{{"type":"toolCall","id":"call-1","name":"read","#,
                    r#""arguments":{{}},"namespace":1}}],"api":"openai-completions","#,
                    r#""provider":"openai-compatible","model":"test",{usage},"#,
                    r#""stopReason":"toolUse","timestamp":1}}}}"#,
                ),
                BASE = BASE,
                usage = usage,
            ),
        ),
        (
            "usage",
            format!(
                concat!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[],"#,
                    r#""api":"openai-completions","provider":"openai-compatible","model":"test","#,
                    r#""usage":{{"input":"zero"}},"stopReason":"stop","timestamp":1}}}}"#,
                ),
                BASE = BASE,
            ),
        ),
        (
            "deferred handle",
            format!(
                concat!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[],"#,
                    r#""api":"openai-completions","provider":"openai-compatible","model":"test","#,
                    r#"{usage},"stopReason":"deferred","deferred":{{"provider":1}},"#,
                    r#""timestamp":1}}}}"#,
                ),
                BASE = BASE,
                usage = usage,
            ),
        ),
        (
            "diagnostic timestamp",
            format!(
                concat!(
                    r#"{{"type":"message",{BASE},"message":{{"role":"assistant","content":[],"#,
                    r#""api":"openai-completions","provider":"openai-compatible","model":"test","#,
                    r#"{usage},"stopReason":"stop","diagnostics":[{{"type":"warning","#,
                    r#""timestamp":"now"}}],"timestamp":1}}}}"#,
                ),
                BASE = BASE,
                usage = usage,
            ),
        ),
        (
            "tool result flag",
            format!(
                r#"{{"type":"message",{BASE},"message":{{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[],"isError":"false","timestamp":1}}}}"#
            ),
        ),
        (
            "null bash exit code",
            format!(
                r#"{{"type":"message",{BASE},"message":{{"role":"bashExecution","command":"pwd","output":"/workspace","exitCode":null,"cancelled":false,"truncated":false,"timestamp":1}}}}"#
            ),
        ),
        (
            "compaction boundary",
            format!(r#"{{"type":"compaction",{BASE},"summary":"summary","tokensBefore":1}}"#),
        ),
        (
            "compaction tail",
            format!(
                r#"{{"type":"compaction",{BASE},"summary":"summary","tokensBefore":1,"retainedTail":{{}}}}"#
            ),
        ),
        (
            "branch from id",
            format!(r#"{{"type":"branch_summary",{BASE},"fromId":1,"summary":"summary"}}"#),
        ),
        (
            "custom display",
            format!(
                r#"{{"type":"custom_message",{BASE},"customType":"fixture","content":"text","display":"true"}}"#
            ),
        ),
        (
            "label target",
            format!(r#"{{"type":"label",{BASE},"targetId":null}}"#),
        ),
        (
            "session name",
            format!(r#"{{"type":"session_info",{BASE},"name":1}}"#),
        ),
    ];
    assert_eq!(cases.len(), 24);
    for (name, input) in cases {
        let error = decode_pi_entry(input.as_bytes()).expect_err(name);
        assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}: {error}");
    }
});

test!(decode_pi_v3_rejects_unsupported_headers, {
    let cases = [
        ("old Otto", r#"{"type":"header","header":{"version":1}}"#),
        (
            "old Pi",
            r#"{"type":"session","version":2,"id":"id","timestamp":"now","cwd":"/workspace"}"#,
        ),
        (
            "future Pi",
            r#"{"type":"session","version":4,"id":"id","timestamp":"now","cwd":"/workspace"}"#,
        ),
        (
            "missing v3",
            r#"{"type":"session","id":"id","timestamp":"now","cwd":"/workspace"}"#,
        ),
    ];
    for (name, input) in cases {
        let error = decode_pi_header(input.as_bytes()).expect_err(name);
        assert_eq!(
            error.kind(),
            PiErrorKind::UnsupportedFormat,
            "case {name}: {error}"
        );
    }
});

test!(decode_pi_v3_rejects_malformed_headers, {
    let cases = [
        ("not JSON", "not-json"),
        (
            "null version",
            r#"{"type":"session","version":null,"id":"id","timestamp":"now","cwd":"/workspace"}"#,
        ),
        (
            "missing id",
            r#"{"type":"session","version":3,"timestamp":"now","cwd":"/workspace"}"#,
        ),
        (
            "wrong timestamp",
            r#"{"type":"session","version":3,"id":"id","timestamp":1,"cwd":"/workspace"}"#,
        ),
        (
            "parent session",
            r#"{"type":"session","version":3,"id":"id","timestamp":"now","cwd":"/workspace","parentSession":false}"#,
        ),
        (
            "trailing JSON",
            r#"{"type":"session","version":3,"id":"id","timestamp":"now","cwd":"/workspace"}{}"#,
        ),
        ("top-level array", "[]"),
    ];
    for (name, input) in cases {
        let error = decode_pi_header(input.as_bytes()).expect_err(name);
        assert_eq!(error.kind(), PiErrorKind::Invalid, "case {name}: {error}");
    }
});

test!(decode_pi_v3_rejects_oversized_entry, {
    let data = vec![b'x'; MAX_SESSION_ENTRY_BYTES + 1];
    let error = decode_pi_file(&data).expect_err("oversized entry accepted");
    assert_eq!(error.kind(), PiErrorKind::EntryTooLarge);
});

test!(decode_pi_v3_rejects_oversized_file, {
    // The claim under test is that the declared length alone decides,
    // before any record is parsed. The buffer below holds no valid JSON at
    // all.
    let data = vec![0u8; MAX_SESSION_FILE_BYTES + 1];
    let error = decode_pi_file(&data).expect_err("oversized file accepted");
    assert_eq!(error.kind(), PiErrorKind::FileTooLarge);
});

test!(
    encode_pi_record_rejects_oversized_raw_record_before_decoding,
    {
        let entry = PiEntry {
            raw: vec![b' '; MAX_SESSION_ENTRY_BYTES + 1],
            ..PiEntry::default()
        };
        let error = encode_pi_record(PiRecord::Entry(&entry)).expect_err("oversized raw accepted");
        assert_eq!(error.kind(), PiErrorKind::EntryTooLarge);
    }
);

test!(encode_pi_record_returns_error_for_invalid_typed_payload, {
    // A typed payload can fail its self-check in more than one way.
    // `RawValue` cannot hold invalid JSON, so this case uses a user message
    // with no content at all.
    let entry = PiEntry {
        type_name: "message".into(),
        id: "e0000001".into(),
        timestamp: "2026-08-27T12:00:01Z".into(),
        message: Some(Box::new(PiMessage {
            role: "user".into(),
            timestamp: 1,
            ..PiMessage::default()
        })),
        ..PiEntry::default()
    };
    let error = encode_pi_record(PiRecord::Entry(&entry)).expect_err("invalid payload accepted");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
});

test!(encode_pi_record_uses_exact_field_names, {
    let header = encode_pi_record(PiRecord::Header(&PiHeader {
        type_name: "session".into(),
        version: PI_SESSION_VERSION,
        id: "950e8400-e29b-41d4-a716-446655440000".into(),
        timestamp: "2026-08-27T12:00:00Z".into(),
        cwd: "/workspace".into(),
        ..PiHeader::default()
    }))
    .expect("encode header");
    assert_raw_json_fields(&header, &["type", "version", "id", "timestamp", "cwd"]);

    let entry = encode_pi_record(PiRecord::Entry(&PiEntry {
        type_name: "message".into(),
        id: "e0000001".into(),
        parent_id: None,
        timestamp: "2026-08-27T12:00:01Z".into(),
        message: Some(Box::new(PiMessage {
            role: "user".into(),
            content: Some(
                serde_json::value::RawValue::from_string(r#""hello""#.into()).expect("raw"),
            ),
            timestamp: 1_787_832_001_000,
            ..PiMessage::default()
        })),
        ..PiEntry::default()
    }))
    .expect("encode entry");
    assert_raw_json_fields(&entry, &["type", "id", "parentId", "timestamp", "message"]);
    let text = String::from_utf8(entry).expect("utf-8");
    assert!(
        !text.contains("parent_id"),
        "entry used non-Pi casing: {text}"
    );
});

fn assert_raw_json_fields(raw: &[u8], want: &[&str]) {
    let object = decode_object(raw, "test").expect("object");
    assert_eq!(
        object.len(),
        want.len(),
        "fields = {:?}, want {want:?}",
        object.keys().collect::<Vec<_>>()
    );
    for field in want {
        assert!(object.contains_key(*field), "field {field} missing");
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn pi_v3_fixtures_are_exact_lf_delimited_json() {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/session/pi-v3");
    let mut names: Vec<String> = std::fs::read_dir(&directory)
        .expect("fixture directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 7, "fixture count");
    assert_eq!(
        names,
        FIXTURES
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect::<Vec<_>>(),
        "embedded fixture list is stale"
    );

    for (name, contents) in FIXTURES {
        assert_eq!(
            *contents,
            std::fs::read(directory.join(name))
                .expect("read fixture")
                .as_slice(),
            "{name} on disk differs from the embedded copy"
        );
        assert!(contents.ends_with(b"\n"), "{name} does not end with LF");
        assert!(!contents.contains(&b'\r'), "{name} contains CR");
        for (index, line) in contents
            .strip_suffix(b"\n")
            .expect("LF")
            .split(|byte| *byte == b'\n')
            .enumerate()
        {
            serde_json::from_slice::<serde_json::Value>(line)
                .unwrap_or_else(|_| panic!("{name} line {} is not JSON", index + 1));
        }
    }
}
