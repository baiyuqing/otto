use crate::model::Block;

use super::*;

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).expect("valid JSON")
}

fn call(id: &str, name: &str, arguments: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_name: name.into(),
            tool_call_id: id.into(),
            arguments: Some(raw(arguments)),
            ..Block::default()
        }],
        ..Message::default()
    }
}

fn result(id: &str, name: &str, is_error: bool) -> Message {
    Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            text: "ok".into(),
            tool_name: name.into(),
            tool_call_id: id.into(),
            is_error,
            ..Block::default()
        }],
        ..Message::default()
    }
}

#[test]
fn paired_successful_calls_produce_the_two_path_lists() {
    let details = derive_compaction_file_details(
        &[
            call("c1", "read", r#"{"path":"./a.txt"}"#),
            result("c1", "read", false),
            call("c2", "write", r#"{"path":"b/../b.txt"}"#),
            result("c2", "write", false),
        ],
        &CompactionDetails::default(),
    );
    assert_eq!(details.read_files, ["a.txt"]);
    assert_eq!(details.modified_files, ["b.txt"]);
}

#[test]
fn a_modified_path_is_not_also_reported_as_read() {
    let details = derive_compaction_file_details(
        &[
            call("c1", "read", r#"{"path":"a.txt"}"#),
            result("c1", "read", false),
            call("c2", "edit", r#"{"path":"a.txt"}"#),
            result("c2", "edit", false),
        ],
        &CompactionDetails::default(),
    );
    assert!(details.read_files.is_empty());
    assert_eq!(details.modified_files, ["a.txt"]);
}

#[test]
fn unpaired_failed_and_mismatched_results_record_nothing() {
    let details = derive_compaction_file_details(
        &[
            call("c1", "read", r#"{"path":"failed.txt"}"#),
            result("c1", "read", true),
            call("c2", "read", r#"{"path":"mismatch.txt"}"#),
            result("c2", "write", false),
            call("c3", "read", r#"{"path":"orphan.txt"}"#),
            result("c9", "read", false),
        ],
        &CompactionDetails::default(),
    );
    assert!(details.read_files.is_empty(), "{:?}", details.read_files);
    assert!(details.modified_files.is_empty());
}

#[test]
fn a_user_message_between_a_call_and_its_result_breaks_the_pair() {
    let details = derive_compaction_file_details(
        &[
            call("c1", "read", r#"{"path":"a.txt"}"#),
            Message {
                role: Role::User,
                blocks: vec![Block::text("wait")],
                ..Message::default()
            },
            result("c1", "read", false),
        ],
        &CompactionDetails::default(),
    );
    assert!(details.read_files.is_empty());
}

#[test]
fn ambiguous_arguments_record_nothing() {
    for arguments in [
        r#"{"path":"a.txt","path":"b.txt"}"#,
        r#"{"path":12}"#,
        r#"{"other":"a.txt"}"#,
        r#"["a.txt"]"#,
        "null",
        r#""a.txt""#,
        r#"{"path":""}"#,
    ] {
        let details = derive_compaction_file_details(
            &[call("c1", "read", arguments), result("c1", "read", false)],
            &CompactionDetails::default(),
        );
        assert!(
            details.read_files.is_empty(),
            "{arguments} should record nothing"
        );
    }
}

#[test]
fn previous_paths_are_carried_forward_and_normalized() {
    let details = derive_compaction_file_details(
        &[],
        &CompactionDetails {
            read_files: vec!["./old.txt".into(), "bad\u{1}".into()],
            modified_files: vec!["dir/../kept.txt".into()],
            omitted_read_files: 2,
            omitted_modified_files: -5,
            ..CompactionDetails::default()
        },
    );
    assert_eq!(details.read_files, ["old.txt"]);
    assert_eq!(details.modified_files, ["kept.txt"]);
    assert_eq!(details.omitted_read_files, 2);
    assert_eq!(details.omitted_modified_files, 0);
}

#[test]
fn the_path_count_bound_counts_what_it_dropped() {
    let mut messages = Vec::new();
    for index in 0..FILE_DETAILS_MAXIMUM_PATHS + 3 {
        let id = format!("c{index}");
        messages.push(call(&id, "read", &format!(r#"{{"path":"f{index:05}"}}"#)));
        messages.push(result(&id, "read", false));
    }
    let details = derive_compaction_file_details(&messages, &CompactionDetails::default());
    assert_eq!(details.read_files.len(), FILE_DETAILS_MAXIMUM_PATHS);
    assert_eq!(details.omitted_read_files, 3);
}

#[test]
fn file_blocks_render_and_strip_round_trip() {
    let details = CompactionDetails {
        read_files: vec!["a.txt".into()],
        modified_files: vec!["b.txt".into()],
        ..CompactionDetails::default()
    };
    let complete =
        append_compaction_blocks("summary body", &[], &[], &[], &details).expect("valid");
    assert_eq!(
        complete,
        "summary body\n\n<read-files>\na.txt\n</read-files>\n\n<modified-files>\nb.txt\n</modified-files>"
    );
    assert_eq!(strip_compaction_blocks(&complete), "summary body");

    let replaced =
        append_compaction_blocks(&complete, &[], &[], &[], &CompactionDetails::default())
            .expect("valid");
    assert_eq!(replaced, "summary body");
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

#[test]
fn user_messages_render_strip_and_parse_round_trip() {
    let messages = vec!["plain".to_owned(), "two\nlines <tag> & \u{85}".to_owned()];
    let details = CompactionDetails {
        read_files: vec!["a.txt".into()],
        ..CompactionDetails::default()
    };
    let complete =
        append_compaction_blocks("summary body", &messages, &[], &[], &details).expect("valid");
    assert_eq!(
        complete,
        "summary body\n\n<user-messages>\n\"plain\"\n\
             \"two\\nlines \\u003ctag\\u003e \\u0026 \\u0085\"\n</user-messages>\n\n\
             <read-files>\na.txt\n</read-files>"
    );
    assert_eq!(strip_compaction_blocks(&complete), "summary body");
    assert_eq!(compaction_user_messages(&complete), messages);
    assert!(compaction_user_messages("summary body").is_empty());
}

#[test]
fn carried_user_messages_keep_the_newest_within_the_byte_bound() {
    // Each previous message encodes to a 1,002-byte line plus its newline.
    let previous: Vec<String> = (0..20)
        .map(|index| format!("{index:02}{}", "p".repeat(998)))
        .collect();
    let image_only = Message {
        role: Role::User,
        blocks: vec![Block {
            block_type: BlockType::Image,
            mime_type: "image/png".into(),
            ..Block::default()
        }],
        ..Message::default()
    };
    let reply = Message {
        role: Role::Assistant,
        blocks: vec![Block::text("not carried")],
        ..Message::default()
    };
    let carried = carry_user_messages(&previous, &[image_only, reply, user("newest")]);
    assert_eq!(carried.len(), 17, "16 previous lines and the newest fit");
    assert_eq!(carried[0], previous[4]);
    assert_eq!(carried[16], "newest");
}

#[test]
fn a_long_user_message_is_truncated_once() {
    let long = "é".repeat(USER_MESSAGE_MAXIMUM_RUNES + 5);
    let carried = carry_user_messages(&[], &[user(&long)]);
    assert!(carried[0].ends_with(USER_MESSAGE_TRUNCATION_MARKER));
    assert_eq!(carry_user_messages(&carried, &[]), carried);
}

#[test]
fn the_plan_block_carries_the_current_plan_and_is_replaced_not_repeated() {
    use crate::session::{PlanStatus, PlanStep};
    let plan = vec![
        PlanStep {
            text: "read".into(),
            status: PlanStatus::Completed,
        },
        PlanStep {
            text: "write".into(),
            status: PlanStatus::InProgress,
        },
    ];
    let complete = append_compaction_blocks(
        "summary body",
        &["hi".to_owned()],
        &[],
        &plan,
        &CompactionDetails::default(),
    )
    .expect("valid");
    assert_eq!(
        complete,
        "summary body\n\n<user-messages>\n\"hi\"\n</user-messages>\n\n<plan>\n[x] read\n[>] write\n</plan>"
    );
    assert_eq!(strip_compaction_blocks(&complete), "summary body");
    assert_eq!(compaction_user_messages(&complete), ["hi"]);

    let newer = &plan[..1];
    let replaced =
        append_compaction_blocks(&complete, &[], &[], newer, &CompactionDetails::default())
            .expect("valid");
    assert_eq!(replaced, "summary body\n\n<plan>\n[x] read\n</plan>");
    let cleared = append_compaction_blocks(&replaced, &[], &[], &[], &CompactionDetails::default())
        .expect("valid");
    assert_eq!(cleared, "summary body");
}

#[test]
fn capability_updates_round_trip_and_follow_the_user_messages_block() {
    let updates = vec!["<n>\nAdded: a\n</n>".to_owned()];
    let complete = append_compaction_blocks(
        "summary body",
        &["hi".to_owned()],
        &updates,
        &[],
        &CompactionDetails {
            read_files: vec!["a.txt".into()],
            ..CompactionDetails::default()
        },
    )
    .expect("valid");
    assert_eq!(
        complete,
        "summary body\n\n<user-messages>\n\"hi\"\n</user-messages>\n\n\
             <capability-updates>\n\"\\u003cn\\u003e\\nAdded: a\\n\\u003c/n\\u003e\"\n</capability-updates>\n\n\
             <read-files>\na.txt\n</read-files>"
    );
    assert_eq!(strip_compaction_blocks(&complete), "summary body");
    assert_eq!(compaction_capability_updates(&complete), updates);
    assert_eq!(compaction_user_messages(&complete), ["hi"]);
    assert!(compaction_capability_updates("summary body").is_empty());
}

fn notice(text: &str) -> Message {
    Message {
        role: Role::Context,
        context_type: CAPABILITY_UPDATE_CONTEXT_TYPE.into(),
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

#[test]
fn an_oversized_notice_is_cut_and_marked_once() {
    let long = "é".repeat(CAPABILITY_UPDATE_MAXIMUM_RUNES + 5);
    let carried = carry_capability_updates(&[], &[notice(&long), user("not a notice")]);
    assert_eq!(carried.len(), 1);
    assert!(carried[0].ends_with(CAPABILITY_UPDATE_TRUNCATION_MARKER));
    assert_eq!(carry_capability_updates(&carried, &[]), carried);
}

#[test]
fn the_oldest_notices_are_dropped_at_the_byte_bound() {
    // Each line encodes to 5,002 bytes plus its newline: three of them
    // and the short newest line fit in 16 KiB, a fourth does not.
    let previous: Vec<String> = (0..4)
        .map(|n| format!("{n}{}", "p".repeat(4_999)))
        .collect();
    let carried = carry_capability_updates(&previous, &[notice("newest")]);
    assert_eq!(carried.len(), 4);
    assert_eq!(carried[0], previous[1]);
    assert_eq!(carried[3], "newest");
}

#[test]
fn an_empty_summary_is_rejected() {
    assert_eq!(
        append_compaction_blocks("  \n ", &[], &[], &[], &CompactionDetails::default())
            .unwrap_err(),
        "complete compaction summary must be nonempty UTF-8"
    );
    let only_blocks = "\n\n<read-files>\na.txt\n</read-files>";
    assert_eq!(
        append_compaction_blocks(only_blocks, &[], &[], &[], &CompactionDetails::default())
            .unwrap_err(),
        "complete compaction summary must be nonempty UTF-8"
    );
}

#[test]
fn file_blocks_inside_an_open_fence_are_not_stripped() {
    let fenced = "body\n```\n\n<read-files>\na.txt\n</read-files>";
    assert_eq!(strip_compaction_blocks(fenced), fenced);
}

#[test]
fn a_block_with_an_empty_or_control_path_is_not_stripped() {
    let broken = "body\n\n<read-files>\n\n</read-files>";
    assert_eq!(strip_compaction_blocks(broken), broken);
    let control = "body\n\n<read-files>\na\u{1}b\n</read-files>";
    assert_eq!(strip_compaction_blocks(control), control);
}

#[test]
fn an_oversized_result_is_rejected() {
    let body = "x".repeat(SUMMARY_MAXIMUM_BYTES - 8);
    let details = CompactionDetails {
        read_files: vec!["a.txt".into()],
        ..CompactionDetails::default()
    };
    assert_eq!(
        append_compaction_blocks(&body, &[], &[], &[], &details).unwrap_err(),
        format!("complete compaction summary exceeds {SUMMARY_MAXIMUM_BYTES} bytes")
    );
}

#[test]
fn path_cleaning_matches_go_filepath_clean() {
    for (input, expected) in [
        ("a/./b", "a/b"),
        ("a//b", "a/b"),
        ("a/b/../c", "a/c"),
        ("../a", "../a"),
        ("/../a", "/a"),
        ("./", "."),
        ("/", "/"),
    ] {
        assert_eq!(clean_path(input), expected, "clean {input}");
    }
    assert_eq!(normalize_detail_path("./"), Some(".".into()));
    assert_eq!(normalize_detail_path(""), None);
    assert_eq!(normalize_detail_path("a\nb"), None);
}
