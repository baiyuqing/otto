use otto_core::model::{Block, BlockType, Message, Role};
use unicode_width::UnicodeWidthStr;

use super::*;

fn row_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn rows(lines: &[Line<'_>]) -> Vec<String> {
    lines.iter().map(row_text).collect()
}

fn entry(kind: EntryKind, raw: &str) -> Entry {
    Entry {
        id: "e1".to_string(),
        kind: Some(kind),
        raw: raw.to_string(),
        ..Entry::default()
    }
}

fn tool_entry(name: &str, args: &str, output: &str) -> Entry {
    Entry {
        id: "t1".to_string(),
        kind: Some(EntryKind::Tool),
        tool_call_id: "call-1".to_string(),
        tool_name: name.to_string(),
        tool_args: args.to_string(),
        tool_output: output.to_string(),
        tool_done: true,
        ..Entry::default()
    }
}

#[test]
fn tool_arguments_display_unicode_but_preserve_literal_escapes_and_controls() {
    let args =
        r#"{"路径":"中文/\ud83d\ude80\u0026\u003c\u003e","literal":"\\u0026","control":"\u001b"}"#;
    let entry = tool_entry("bash", args, "");
    for details in [false, true] {
        let text = rows(&entry_lines(&entry, details, 200)).join("\n");
        assert_eq!(text.contains("中文/🚀&<>"), details, "{text}");
        if details {
            assert!(text.contains(r#"\\u0026"#), "{text}");
        }
        assert!(!text.contains('\u{1b}'), "raw terminal escape");
    }
    let text = rows(&entry_lines(&entry, true, 200)).join("\n");
    assert!(text.contains(r#"\u001b"#), "{text}");
}

#[test]
fn a_wrapped_prompt_is_marked_once_and_then_aligned() {
    let rendered = entry_lines(&entry(EntryKind::User, "alpha bravo charlie"), false, 12);
    let rows = rows(&rendered);
    assert!(rows.len() > 1, "expected a wrapped prompt");
    assert!(rows[0].starts_with(USER_MARK), "{:?}", rows[0]);
    for row in &rows[1..] {
        assert!(
            row.starts_with("  "),
            "{row:?} is not aligned under the marker"
        );
        assert!(!row.starts_with(USER_MARK), "{row:?} repeats the marker");
    }
}

#[test]
fn a_prompt_is_green_including_its_marker() {
    let rendered = entry_lines(&entry(EntryKind::User, "hi"), false, 40);
    for span in &rendered[0].spans {
        assert_eq!(span.style.fg, Some(Color::Green));
    }
}

#[test]
fn a_multi_line_prompt_keeps_its_own_line_breaks_under_one_marker() {
    let rendered = entry_lines(&entry(EntryKind::User, "one\ntwo"), false, 40);
    let padded = rows(&rendered);
    let rows: Vec<&str> = padded.iter().map(|row| row.trim_end()).collect();
    assert_eq!(rows, ["❯ one", "  two"]);
}

#[test]
fn a_prompt_is_one_full_width_band_on_every_row() {
    let rendered = entry_lines(&entry(EntryKind::User, "alpha bravo charlie"), false, 12);
    assert!(rendered.len() > 1, "expected a wrapped prompt");
    for line in &rendered {
        assert_eq!(
            row_text(line).width(),
            12,
            "a row short of the width breaks the band: {:?}",
            row_text(line)
        );
        for span in &line.spans {
            assert_eq!(
                span.style.bg,
                Some(PROMPT_BACKGROUND),
                "{:?} is not part of the band",
                span.content
            );
        }
    }
}

#[test]
fn a_cjk_prompt_row_is_banded_to_the_last_column() {
    // Width 9 leaves a column no wide character fits into, which only
    // the padding can close.
    let rendered = entry_lines(&entry(EntryKind::User, "集群数量极多"), false, 9);
    let visible = rows(&rendered);
    assert_eq!(
        visible.iter().map(|row| row.trim_end()).collect::<Vec<_>>(),
        ["❯ 集群数", "  量极多"],
        "the CJK prompt must render both rows"
    );
    for line in &rendered {
        assert_eq!(row_text(line).width(), 9, "{:?}", row_text(line));
        for span in &line.spans {
            assert_eq!(span.style.bg, Some(PROMPT_BACKGROUND));
        }
    }
}

#[test]
fn reasoning_text_is_dimmed() {
    let rendered = entry_lines(&entry(EntryKind::Reasoning, "weigh **options**"), false, 40);
    assert!(!rendered.is_empty());
    for span in rendered.iter().flat_map(|line| &line.spans) {
        assert!(
            span.style.add_modifier.contains(Modifier::DIM),
            "{:?} is not dim",
            span.content
        );
    }
}

#[test]
fn a_reply_is_not_banded_or_padded() {
    let rendered = entry_lines(
        &entry(EntryKind::Assistant, "it guards the query"),
        false,
        40,
    );
    assert_eq!(
        rows(&rendered),
        ["⏺ it guards the query"],
        "reply text must remain visible"
    );
    for line in &rendered {
        assert!(
            row_text(line).width() < 40,
            "{:?} was padded",
            row_text(line)
        );
        for span in &line.spans {
            assert_eq!(span.style.bg, None, "{:?} is banded", span.content);
        }
    }
}

#[test]
fn a_reply_gets_one_bullet_and_an_aligned_indent() {
    let rendered = entry_lines(
        &entry(EntryKind::Assistant, "alpha bravo charlie"),
        false,
        12,
    );
    let rows = rows(&rendered);
    assert!(rows.len() > 1, "expected a wrapped reply");
    assert!(rows[0].starts_with(BULLET_MARK), "{:?}", rows[0]);
    for row in &rows[1..] {
        assert!(
            row.starts_with("  "),
            "{row:?} is not aligned under the bullet"
        );
        assert!(!row.starts_with(BULLET_MARK), "{row:?} repeats the bullet");
    }
}

#[test]
fn a_reply_keeps_its_bullet_across_its_own_paragraphs() {
    let rendered = entry_lines(&entry(EntryKind::Assistant, "one\n\ntwo"), false, 40);
    let rows = rows(&rendered);
    assert_eq!(rows.first().map(String::as_str), Some("⏺ one"));
    assert_eq!(
        rows.last().map(String::as_str),
        Some("  two"),
        "both reply paragraphs must be visible: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.starts_with(BULLET_MARK))
            .count(),
        1,
        "only the first row of an entry is bulleted: {rows:?}"
    );
}

#[test]
fn a_prompt_and_the_reply_after_it_are_separated_and_distinct() {
    let rendered = lines(
        &[
            entry(EntryKind::User, "what does this do"),
            entry(EntryKind::Assistant, "it guards the query"),
        ],
        false,
        40,
    );
    let rows = rows(&rendered);
    assert_eq!(rows[0].trim_end(), "❯ what does this do");
    assert_eq!(rows[1], "");
    assert_eq!(rows[2], "⏺ it guards the query");
}

#[test]
fn an_empty_entry_renders_nothing() {
    assert!(entry_lines(&entry(EntryKind::Assistant, ""), false, 40).is_empty());
    assert!(lines(&[entry(EntryKind::Assistant, "")], false, 40).is_empty());
}

#[test]
fn a_prompt_wraps_by_display_column_not_character_count() {
    let rendered = entry_lines(&entry(EntryKind::User, "集群数量极多"), false, 8);
    assert_eq!(rows(&rendered), ["❯ 集群数", "  量极多"]);
}

#[test]
fn cancelled_execution_keeps_timing_identity_and_raw_details_at_narrow_width() {
    let mut call = tool_entry(
        "bash",
        r#"{"command":"private command"}"#,
        "cancelled output",
    );
    call.id = "operation:op_cancelled".into();
    call.operation_id = Some("op_cancelled".into());
    call.disposition = Some(OperationDisposition::Cancelled);
    call.effect_certainty = Some(EffectCertainty::Unknown);
    call.tool_error = true;
    call.tool_elapsed = Some(std::time::Duration::from_millis(1250));
    let compact = rows(&entry_lines(&call, false, 26)).join("\n");
    assert!(compact.contains("cancelled"));
    assert!(compact.contains("1.2s"));
    assert!(!compact.contains("private command"));
    let expanded = rows(&entry_lines(&call, true, 26)).join("\n");
    // Wrapping may insert gutter indentation; compare the full rendered text
    // without whitespace to prove IDs and raw data are not truncated.
    let flat: String = expanded.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(flat.contains("operation:op_cancelled"));
    assert!(flat.contains(&call.tool_call_id));
    assert!(flat.contains("privatecommand"));
    assert!(expanded.contains("execution 1.2s"));
}

#[test]
fn a_folded_tool_call_shows_its_name_and_a_result_under_it() {
    let rendered = entry_lines(
        &tool_entry("bash", "{\"command\":\"ls\"}", "one"),
        false,
        60,
    );
    let rows = rows(&rendered);
    assert_eq!(rows[0], format!("⏺ bash [{}]", compact_invocation_id("t1")));
    assert_eq!(rows[1], "  ⎿ completed · 1 lines");
}

#[test]
fn expanded_file_edits_show_colored_hunks() {
    let output = "edited src/main.rs\n@@ -1,2 +1,2 @@\n old\n-before\n+after\n note";
    let rendered = entry_lines(&tool_entry("edit", "", output), true, 80);
    let text = rows(&rendered);
    assert!(text.iter().any(|line| line.contains("-before")), "{text:?}");
    assert!(text.iter().any(|line| line.contains("+after")), "{text:?}");
    let line_style = |needle: &str| {
        rendered
            .iter()
            .find(|line| row_text(line).contains(needle))
            .unwrap()
            .spans
            .last()
            .unwrap()
            .style
            .fg
    };
    assert_eq!(line_style("@@ -1"), Some(Color::Cyan));
    assert_eq!(line_style("-before"), Some(Color::Red));
    assert_eq!(line_style("+after"), Some(Color::Green));

    let write = entry_lines(
        &tool_entry("write", "", "wrote x (2 bytes)\n@@ -0,0 +1,1 @@\n+x"),
        true,
        80,
    );
    assert!(rows(&write).iter().any(|line| line.contains("+x")));
}

#[test]
fn a_resumed_write_keeps_its_recorded_diff() {
    let history = [
        Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "write".into(),
                ..Block::default()
            }],
            ..Message::default()
        },
        Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call-1".into(),
                tool_name: "write".into(),
                text: "wrote x (2 bytes)\n@@ -0,0 +1,1 @@\n+x".into(),
                ..Block::default()
            }],
            ..Message::default()
        },
    ];
    let (entries, _) = super::super::entries::entries_from_history(&history);
    let rendered = lines(&entries, true, 80);
    assert!(rows(&rendered).iter().any(|line| line.contains("+x")));
}

#[test]
fn failed_edits_keep_the_usual_fold_and_diff_text_escapes_controls() {
    let mut failed = tool_entry("edit", "", "edit failed\nline 2\nline 3\nline 4");
    failed.tool_error = true;
    let text = rows(&entry_lines(&failed, false, 80)).join("\n");
    assert!(text.contains("error · edit failed"), "{text}");
    assert!(!text.contains("line 2"));
    let output = "edited x\n@@ -1,1 +1,1 @@\n-old\n+new\u{1b}[31m";
    let joined = rows(&entry_lines(&tool_entry("edit", "", output), true, 80)).join("\n");
    assert!(joined.contains(r"\x1b"), "{joined}");
    assert!(!joined.contains('\u{1b}'), "{joined}");
}

#[test]
fn a_folded_tool_call_bullet_is_cyan_and_its_name_bold() {
    let rendered = entry_lines(&tool_entry("read", "", "ok"), false, 60);
    assert_eq!(rendered[0].spans[0].style.fg, Some(Color::Cyan));
    assert!(
        rendered[0].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn a_folded_tool_call_shows_at_most_three_result_lines() {
    let rendered = entry_lines(&tool_entry("bash", "", "l1\nl2\nl3\nl4\nl5\nl6"), false, 60);
    assert_eq!(
        rows(&rendered),
        [
            &format!("⏺ bash [{}]", compact_invocation_id("t1")),
            "  ⎿ completed · 6 lines"
        ]
    );
}

#[test]
fn a_folded_tool_call_cuts_a_long_result_line() {
    let long = format!("sample result: {}", "x".repeat(200));
    let mut call = tool_entry("bash", "", &long);
    call.tool_error = true;
    let rendered = entry_lines(&call, false, 200);
    let rows = rows(&rendered);
    assert!(
        rows[1].contains("sample result: "),
        "preview must retain the beginning: {:?}",
        rows[1]
    );
    assert!(rows[1].chars().count() < 100, "{:?}", rows[1]);
    assert!(rows[1].ends_with('…'), "{:?}", rows[1]);
}

#[test]
fn a_tool_call_with_no_output_says_so() {
    let rendered = entry_lines(&tool_entry("write", "", "  \n "), false, 60);
    assert_eq!(rows(&rendered)[1], "  ⎿ completed · no output");
}

#[test]
fn a_running_tool_call_has_explicit_status() {
    let mut call = tool_entry("bash", "", "");
    call.tool_done = false;
    let rendered = entry_lines(&call, false, 60);
    assert_eq!(
        rows(&rendered),
        [
            format!("⏺ bash [{}]", compact_invocation_id("t1")),
            "  ⎿ running".into()
        ]
    );
}

#[test]
fn a_failed_tool_call_shows_its_result_in_red() {
    let mut call = tool_entry("bash", "", "boom");
    call.tool_error = true;
    let rendered = entry_lines(&call, false, 60);
    assert_eq!(rendered[1].spans[0].style.fg, Some(Color::Red));
}

#[test]
fn unknown_outcome_is_a_warning_even_when_the_tool_errored() {
    let mut call = tool_entry("bash", "", "connection lost");
    call.tool_error = true;
    call.disposition = Some(OperationDisposition::Interrupted);
    call.effect_certainty = Some(EffectCertainty::Unknown);
    let rendered = entry_lines(&call, false, 100);
    assert_eq!(
        rows(&rendered)[1],
        "  ⎿ effect unknown · Outcome unknown — check effects before retrying · connection lost"
    );
    assert_eq!(rendered[1].spans[0].style.fg, Some(Color::Yellow));
    assert!(
        rendered[1].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn completed_error_has_a_known_result_label() {
    let mut call = tool_entry("bash", "", "exit 1");
    call.tool_error = true;
    call.disposition = Some(OperationDisposition::Error);
    call.effect_certainty = Some(EffectCertainty::Completed);
    assert_eq!(
        rows(&entry_lines(&call, false, 100))[1],
        "  ⎿ error · Failed with a known result · exit 1"
    );
}

#[test]
fn legacy_error_preserves_actionable_text_without_fabricating_metadata() {
    let mut call = tool_entry(
        "bash",
        "",
        "Outcome unknown — check effects before retrying",
    );
    call.tool_error = true;
    let rendered = entry_lines(&call, false, 100);
    let text = rows(&rendered).join("\n");
    assert!(
        text.contains("error") && text.contains("check effects before retrying"),
        "{text}"
    );
    assert_eq!(rendered[1].spans[0].style.fg, Some(Color::Red));
}

#[test]
fn expanded_details_show_the_call_id_and_every_output_line() {
    let rendered = entry_lines(&tool_entry("bash", "{}", "l1\nl2\nl3\nl4"), true, 60);
    let rows = rows(&rendered);
    assert_eq!(rows[0], "⏺ bash (t1, provider: call-1)");
    assert_eq!(rows[1].trim(), "{}");
    let output: Vec<_> = rows[2..].iter().map(|row| row.trim()).collect();
    assert_eq!(
        output,
        ["⎿ l1", "l2", "l3", "l4"],
        "expanded output must retain every line in order"
    );
    assert!(
        !rows.iter().any(|row| row.contains("ctrl+o")),
        "expanded output is not counted: {rows:?}"
    );
}

#[test]
fn an_error_entry_is_red() {
    let rendered = entry_lines(&entry(EntryKind::Error, "it broke"), false, 40);
    assert_eq!(rendered[0].spans[0].style.fg, Some(Color::Red));
    assert!(rows(&rendered)[0].starts_with(BULLET_MARK));
}

#[test]
fn a_compaction_entry_uses_the_system_marker() {
    let rendered = entry_lines(&entry(EntryKind::Compaction, "checkpoint"), false, 40);
    assert_eq!(
        rows(&rendered),
        [format!("{SYSTEM_MARK}checkpoint")],
        "compaction text must follow its system marker"
    );
}

#[test]
fn a_narrow_width_does_not_panic() {
    for width in [0usize, 1, 2, 3, 4] {
        let _ = lines(
            &[
                entry(EntryKind::User, "alpha"),
                entry(EntryKind::Assistant, "bravo"),
                tool_entry("bash", "{\"command\":\"ls\"}", "one\ntwo"),
            ],
            false,
            width,
        );
    }
}
#[test]
fn compact_hides_raw_payload_and_unknown_tools_fall_back_to_name() {
    let entry = tool_entry(
        "custom_tool",
        r#"{"secret":"hidden-argument"}"#,
        "hidden-output\n@@ diff",
    );
    let compact = rows(&entry_lines(&entry, false, 80)).join("\n");
    assert!(compact.contains("custom_tool") && compact.contains("completed"));
    for raw in ["hidden-argument", "hidden-output", "@@", "call-1"] {
        assert!(!compact.contains(raw), "{compact}");
    }
    let detail = rows(&entry_lines(&entry, true, 80)).join("\n");
    assert!(detail.contains("hidden-argument") && detail.contains("hidden-output"));
}

#[test]
fn compact_known_target_and_running_status() {
    let mut entry = tool_entry("read", r#"{"path":"src/main.rs","other":"hidden"}"#, "");
    entry.tool_done = false;
    let compact = rows(&entry_lines(&entry, false, 80)).join("\n");
    assert!(compact.contains("src/main.rs · running"), "{compact}");
    assert!(!compact.contains("hidden"));
}
#[test]
fn compact_identity_is_short_stable_and_details_retain_full_identity() {
    let mut call = tool_entry("read", "", "done");
    call.id = "operation:op_a_long_native_identity_123456789".into();
    let label = compact_invocation_id(&call.id);
    assert_eq!(label.len(), 12);
    assert_eq!(label, compact_invocation_id(&call.id));
    assert_ne!(label, compact_invocation_id("operation:op_other"));
    let compact = rows(&entry_lines(&call, false, 100)).join("\n");
    assert!(compact.contains(&format!("[{label}]")));
    assert!(!compact.contains(&call.id));
    let details = rows(&entry_lines(&call, true, 160)).join("\n");
    assert!(details.contains(&call.id) && details.contains(&call.tool_call_id));
}
