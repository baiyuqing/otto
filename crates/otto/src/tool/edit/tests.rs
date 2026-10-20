
use super::*;
use crate::faults::publish::FaultyPublishOps;
use crate::faults::{Fault, FaultError, FaultPlan, Rule, Trigger};
use crate::fsops::PublishStep;
use crate::tool::testutil::{run, workspace};
use otto_core::model::{EffectCertainty, OperationDisposition};

fn publish_fault(step: PublishStep) -> std::sync::Arc<FaultPlan<PublishStep>> {
    FaultPlan::new(vec![Rule::new(
        step,
        Trigger::Always,
        Fault::Before(FaultError::Os(libc::EIO)),
    )])
}

fn sample(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.txt");
    std::fs::write(&path, contents).unwrap();
    (root, path)
}

#[tokio::test]
async fn git_config_is_refused_through_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".git/config"), "old\n").unwrap();
    std::os::unix::fs::symlink(".git/config", root.path().join("link")).unwrap();
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);

    for path in [".git/config", ".GIT/config", "link"] {
        let result = run(
            &tool,
            &format!(r#"{{"path":"{path}","old_text":"old","new_text":"new"}}"#),
        )
        .await;
        assert!(
            result.is_error && result.content.contains("read-only"),
            "{path}: {result:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join(".git/config")).unwrap(),
        "old\n"
    );
}

#[tokio::test]
async fn unknown_fields_and_missing_arguments_are_rejected() {
    let (root, _) = sample("same\n");
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);

    // Malformed JSON and trailing tokens are rejected by the stream
    // decoder before a `RawValue` exists; see `result::tests`.
    let unknown = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"same","new_text":"new","extra":true}"#,
    )
    .await;
    assert!(
        unknown.is_error && unknown.content.contains("unknown field"),
        "{unknown:?}"
    );

    let missing_path = run(&tool, r#"{"old_text":"same","new_text":"new"}"#).await;
    assert!(
        missing_path.is_error && missing_path.content.contains("path"),
        "{missing_path:?}"
    );

    let missing_old = run(&tool, r#"{"path":"sample.txt","new_text":"new"}"#).await;
    assert!(
        missing_old.is_error && missing_old.content.contains("old_text"),
        "{missing_old:?}"
    );
    assert_eq!(
        missing_old.outcome_override.unwrap().effect_certainty,
        EffectCertainty::NotStarted
    );
}

#[tokio::test]
async fn directory_sync_failure_after_edit_rename_is_unknown() {
    let (root, path) = sample("before\n");
    let workspace = workspace(root.path());
    let plan = publish_fault(PublishStep::DirSync);
    let ops = FaultyPublishOps::new(plan.clone());
    let tool = EditTool::with_publish_ops(&workspace, &ops);

    let result = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"before","new_text":"after"}"#,
    )
    .await;
    plan.assert_all_fired();

    assert!(result.is_error, "{result:?}");
    let outcome = result
        .outcome_override
        .expect("post-rename failure override");
    assert_eq!(outcome.disposition, OperationDisposition::Error);
    assert_eq!(outcome.effect_certainty, EffectCertainty::Unknown);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "after\n");
}

#[tokio::test]
async fn rename_failure_keeps_the_file_and_leaves_no_temporary_file() {
    let (root, path) = sample("before\n");
    let workspace = workspace(root.path());
    let plan = publish_fault(PublishStep::Rename);
    let ops = FaultyPublishOps::new(plan.clone());
    let tool = EditTool::with_publish_ops(&workspace, &ops);

    let result = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"before","new_text":"after"}"#,
    )
    .await;

    plan.assert_all_fired();
    assert!(result.is_error, "{result:?}");
    // A plain error result: only a post-rename failure overrides the
    // outcome to `Unknown`.
    assert!(result.outcome_override.is_none(), "{result:?}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "before\n");
    let names: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["sample.txt"]);
}

#[tokio::test]
async fn an_ambiguous_or_absent_match_is_an_error() {
    let (root, _) = sample("same\nsame\n");
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);

    let ambiguous = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"same","new_text":"new"}"#,
    )
    .await;
    assert!(ambiguous.is_error);
    assert_eq!(
        ambiguous
            .outcome_override
            .as_ref()
            .unwrap()
            .effect_certainty,
        EffectCertainty::NotStarted
    );
    assert_eq!(
        ambiguous.content,
        "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\n\
             match 1, line 1:\n1 | same\n2 | same\n\
             match 2, line 2:\n1 | same\n2 | same"
    );

    let absent = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"missing","new_text":"new"}"#,
    )
    .await;
    assert!(absent.is_error);
    assert_eq!(
        absent.outcome_override.as_ref().unwrap().effect_certainty,
        EffectCertainty::NotStarted
    );
    assert_eq!(
        absent.content,
        "edit failed: old_text was not found in sample.txt; no line of old_text matches a line of the file, ignoring leading and trailing whitespace"
    );
}

#[tokio::test]
async fn overlapping_occurrences_are_ambiguous() {
    for (name, file, old_text, line) in [
        ("exact", "aaa\n", "aa", "aaa"),
        (
            "fuzzy",
            "x\u{2019}x\u{2019}x\n",
            "x'x",
            "x\u{2019}x\u{2019}x",
        ),
    ] {
        let (root, path) = sample(file);
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": old_text,
            "new_text": "X",
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        assert!(result.is_error, "{name}: {result:?}");
        assert_eq!(
            result.content,
            format!(
                "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\nmatch 1, line 1:\n1 | {line}\nmatch 2, line 1:\n1 | {line}"
            ),
            "{name}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file, "{name}");
    }
}

#[tokio::test]
async fn match_errors_show_only_bounded_file_lines() {
    let (root, _) = sample("dup\ndup\n");
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);

    let arguments = serde_json::json!({
        "path": "sample.txt",
        "old_text": "ZQX marker ".repeat(2000),
        "new_text": "new",
    })
    .to_string();
    let not_found = run(&tool, &arguments).await;
    assert!(
        not_found.is_error && !not_found.content.contains("ZQX"),
        "{not_found:?}"
    );
    assert!(not_found.content.len() <= 200, "{not_found:?}");

    let (root, _) = sample(&"dup\n".repeat(300));
    let big = crate::tool::testutil::workspace(root.path());
    let ambiguous = run(
        &EditTool::new(&big),
        r#"{"path":"sample.txt","old_text":"dup","new_text":"new"}"#,
    )
    .await;
    assert!(ambiguous.is_error, "{ambiguous:?}");
    assert!(
        ambiguous.content.contains("(295 more matches not shown)"),
        "{ambiguous:?}"
    );
    assert!(
        ambiguous.content.len() <= MAX_DIFF_BYTES + 300,
        "len = {}",
        ambiguous.content.len()
    );

    // Twenty 300-byte lines are cut to 203 bytes each, which exceeds the
    // listing cap.
    let lines: Vec<String> = (0..30)
        .map(|index| format!("{index:02}{}", "q".repeat(300)))
        .collect();
    let (root, _) = sample(&format!("{}\n", lines.join("\n")));
    let big = crate::tool::testutil::workspace(root.path());
    let arguments = serde_json::json!({
        "path": "sample.txt",
        "old_text": format!("{}\nmissing", lines.join("\n")),
        "new_text": "new",
    })
    .to_string();
    let capped = run(&EditTool::new(&big), &arguments).await;
    assert!(capped.is_error, "{capped:?}");
    let (_, listing) = capped.content.split_once(":\n").unwrap();
    assert!(listing.ends_with("\n... (truncated)"), "{capped:?}");
    assert!(
        listing.len() <= MAX_DIFF_BYTES + "\n... (truncated)".len(),
        "len = {}",
        listing.len()
    );
}

#[tokio::test]
async fn an_ambiguous_match_lists_lines_around_each_match() {
    let file = "fn a() {\n    let x = 1;\n    one();\n    two();\n}\nfn b() {\n    let x = 1;\n    one();\n    two();\n}\n";
    let (root, path) = sample(file);
    let workspace = workspace(root.path());
    let arguments = serde_json::json!({
        "path": "sample.txt",
        "old_text": "let x = 1;\n    one();\n    two();",
        "new_text": "X",
    })
    .to_string();
    let result = run(&EditTool::new(&workspace), &arguments).await;
    assert!(result.is_error, "{result:?}");
    assert_eq!(
        result.content,
        "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\n\
             match 1, lines 2-4:\n\
             \x201 | fn a() {\n\
             \x202 |     let x = 1;\n\
             ...\n\
             \x204 |     two();\n\
             \x205 | }\n\
             match 2, lines 7-9:\n\
             \x206 | fn b() {\n\
             \x207 |     let x = 1;\n\
             ...\n\
             \x209 |     two();\n\
             10 | }"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), file);
}

#[tokio::test]
async fn an_ambiguous_fuzzy_match_maps_to_file_lines() {
    let (root, _) = sample("a\nsay(\u{201c}hi\u{201d})\nb\nc\nsay(\u{201c}hi\u{201d})\nd\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"say(\"hi\")","new_text":"X"}"#,
    )
    .await;
    assert_eq!(
        result.content,
        "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\n\
             match 1, line 2:\n1 | a\n2 | say(\u{201c}hi\u{201d})\n3 | b\n\
             match 2, line 5:\n4 | c\n5 | say(\u{201c}hi\u{201d})\n6 | d"
    );
}

#[tokio::test]
async fn a_stale_old_text_shows_the_closest_current_lines() {
    let (root, path) = sample(
        "fn alpha() {}\nfn beta() {}\nassert_eq!(a, b);\nfn gamma() {}\nfn delta() {}\nfn omega() {}\n",
    );
    let workspace = workspace(root.path());
    let arguments = serde_json::json!({
        "path": "sample.txt",
        "old_text": "fn alpha() {}\nfn beta() {}\nassert_eq!(\n    a,\n    b\n);",
        "new_text": "X",
    })
    .to_string();
    let result = run(&EditTool::new(&workspace), &arguments).await;
    assert!(result.is_error, "{result:?}");
    assert_eq!(
        result.content,
        "edit failed: old_text was not found in sample.txt. Closest lines in the current file (2 of 6 old_text lines match, ignoring leading and trailing whitespace):\n\
             1 | fn alpha() {}\n\
             2 | fn beta() {}\n\
             3 | assert_eq!(a, b);\n\
             4 | fn gamma() {}\n\
             5 | fn delta() {}\n\
             6 | fn omega() {}"
    );
    assert!(!result.content.contains("assert_eq!(\n"), "{result:?}");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("assert_eq!(a, b);")
    );
}

#[tokio::test]
async fn the_closest_lines_take_the_earliest_tie_and_stop_at_20() {
    let (root, _) = sample("one\nmid\none\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"one\nmissing","new_text":"X"}"#,
    )
    .await;
    assert!(
        result.content.ends_with(":\n1 | one\n2 | mid"),
        "{result:?}"
    );

    let file: String = (1..=30).map(|n| format!("l{n}\n")).collect();
    std::fs::write(root.path().join("sample.txt"), file).unwrap();
    let old_text: Vec<String> = std::iter::once("l1".to_owned())
        .chain((2..=25).map(|n| format!("gone{n}")))
        .collect();
    let arguments = serde_json::json!({
        "path": "sample.txt",
        "old_text": old_text.join("\n"),
        "new_text": "X",
    })
    .to_string();
    let result = run(&EditTool::new(&workspace), &arguments).await;
    let listing: Vec<String> = (1..=20).map(|n| format!("{n:>2} | l{n}")).collect();
    assert!(
            result.content.ends_with(&format!(
                "(1 of 25 old_text lines match, ignoring leading and trailing whitespace):\n{}\n(5 more lines)",
                listing.join("\n")
            )),
            "{result:?}"
        );
}

#[tokio::test]
async fn a_not_found_without_any_matching_line_says_so() {
    let (root, _) = sample("one\ntwo\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"zzz\nyyy","new_text":"X"}"#,
    )
    .await;
    assert_eq!(
        result.content,
        "edit failed: old_text was not found in sample.txt; no line of old_text matches a line of the file, ignoring leading and trailing whitespace"
    );
}

#[tokio::test]
async fn a_batch_match_error_keeps_its_edit_number_and_listing() {
    let (root, path) = sample("a\nb\nc\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"A"},{"old_text":"x\nb","new_text":"B"}]}"#,
        )
        .await;
    assert_eq!(
        result.content,
        "edit 2 failed: old_text was not found in sample.txt. Closest lines in the current file (1 of 2 old_text lines match, ignoring leading and trailing whitespace):\n1 | a\n2 | b"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
}

#[tokio::test]
async fn a_long_file_line_is_cut_in_an_error() {
    let line = format!("AB{}", "z".repeat(300));
    let (root, _) = sample(&format!("{line}\n{line}\n"));
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"AB","new_text":"X"}"#,
    )
    .await;
    let cut = format!("AB{}...", "z".repeat(198));
    assert_eq!(
        result.content,
        format!(
            "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique. Lines around each match:\nmatch 1, line 1:\n1 | {cut}\n2 | {cut}\nmatch 2, line 2:\n1 | {cut}\n2 | {cut}"
        )
    );
}

#[tokio::test]
async fn binary_and_oversized_files_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("binary"), [b'a', 0, b'b']).unwrap();
    let huge = root.path().join("huge.txt");
    std::fs::write(&huge, "x").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&huge)
        .unwrap()
        .set_len(crate::tool::read::MAX_READ_FILE_BYTES + 1)
        .unwrap();
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);

    let binary = run(&tool, r#"{"path":"binary","old_text":"a","new_text":"c"}"#).await;
    assert!(
        binary.is_error && binary.content.contains("binary"),
        "{binary:?}"
    );

    let oversized = run(
        &tool,
        r#"{"path":"huge.txt","old_text":"x","new_text":"y"}"#,
    )
    .await;
    assert!(
        oversized.is_error && oversized.content.contains("too large"),
        "{oversized:?}"
    );
}

#[tokio::test]
async fn an_exact_single_match_is_replaced_and_diffed() {
    let (root, path) = sample("hello world\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"world","new_text":"there"}"#,
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello there\n");
    assert!(result.content.contains("sample.txt"), "{result:?}");
    assert!(result.content.contains("-hello world"), "{result:?}");
    assert!(result.content.contains("+hello there"), "{result:?}");
}

#[tokio::test]
async fn dropping_the_line_break_ending_old_text_adds_a_note() {
    let (root, path) = sample("a\nb\nc\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"a\nb\n","new_text":"x\ny"}"#,
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\nyc\n");
    assert!(
        result
            .content
            .contains("note: old_text ends with a line break but new_text does not"),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_joining_fuzzy_match_in_a_batch_gets_a_numbered_note() {
    let (root, path) = sample("one \ntwo\nthree\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"three","new_text":"3"},{"old_text":"one\n","new_text":"1"}]}"#,
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "1two\n3\n");
    assert!(
        result.content.contains("note: edit 2: old_text ends"),
        "{result:?}"
    );
    assert!(!result.content.contains("edit 1:"), "{result:?}");
}

#[tokio::test]
async fn kept_or_deleted_line_breaks_add_no_note() {
    for (old_text, new_text, expected) in [
        (r"a\nb\n", r"x\ny\n", "x\ny\nc\n"),
        (r"a\nb", r"x\ny", "x\ny\nc\n"),
        (r"a\n", "", "b\nc\n"),
        (r"c\n", "z", "a\nb\nz"),
    ] {
        let (root, path) = sample("a\nb\nc\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            &format!(r#"{{"path":"sample.txt","old_text":"{old_text}","new_text":"{new_text}"}}"#),
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert!(!result.content.contains("note:"), "{result:?}");
    }
}

#[tokio::test]
async fn an_array_of_edits_is_applied() {
    let (root, path) = sample("one\ntwo\nthree\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"one","new_text":"ONE"},{"old_text":"three","new_text":"THREE"}]}"#,
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\nTHREE\n");
}

#[tokio::test]
async fn untyped_edit_shapes_are_rejected() {
    let (root, path) = sample("a b c\n");
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);

    for (name, arguments) in [
        (
            "camel case keys",
            r#"{"path":"sample.txt","oldText":"a","newText":"A"}"#,
        ),
        (
            "string encoded edits",
            r#"{"path":"sample.txt","edits":"[{\"old_text\":\"a\",\"new_text\":\"A\"}]"}"#,
        ),
        (
            "object edits",
            r#"{"path":"sample.txt","edits":{"old_text":"a","new_text":"A"}}"#,
        ),
        (
            "single and list edits",
            r#"{"path":"sample.txt","old_text":"a","new_text":"A","edits":[{"old_text":"c","new_text":"C"}]}"#,
        ),
        ("empty edits", r#"{"path":"sample.txt","edits":[]}"#),
        (
            "edit without new_text",
            r#"{"path":"sample.txt","edits":[{"old_text":"a"}]}"#,
        ),
    ] {
        let result = run(&tool, arguments).await;
        assert!(result.is_error, "{name}: expected an error, got {result:?}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a b c\n");
}

#[tokio::test]
async fn mixed_argument_forms_explain_how_to_retry_without_writing() {
    let (root, path) = sample("a b c\n");
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);
    let result = run(
            &tool,
            r#"{"path":"sample.txt","old_text":"a","new_text":"A","edits":[{"old_text":"c","new_text":"C"}]}"#,
        )
        .await;
    assert!(result.is_error, "{result:?}");
    assert!(
        result
            .content
            .contains("move every replacement into edits and omit top-level old_text/new_text"),
        "{result:?}"
    );
    assert_eq!(
        result.outcome_override.unwrap().effect_certainty,
        EffectCertainty::NotStarted
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a b c\n");

    let retry = run(
            &tool,
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"A"},{"old_text":"c","new_text":"C"}]}"#,
        )
        .await;
    assert!(!retry.is_error, "{retry:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A b C\n");
}

/// A null `edits` array with a single replacement, and the empty array
/// models send in the same placeholder position.
#[tokio::test]
async fn an_empty_edits_key_allows_a_single_replacement() {
    for edits in ["null", "[]"] {
        let (root, path) = sample("a\n");
        let workspace = workspace(root.path());
        let result = run(
            &EditTool::new(&workspace),
            &format!(r#"{{"path":"sample.txt","old_text":"a","new_text":"A","edits":{edits}}}"#),
        )
        .await;
        assert!(!result.is_error, "{edits}: {result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\n");
    }
}

/// The mirror of the empty `edits` array: models emit `""` in the
/// old_text/new_text position they are not using, beside the `edits` they
/// are using.
#[tokio::test]
async fn blank_single_keys_allow_an_array_of_edits() {
    let (root, path) = sample("one\ntwo\nthree\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"","new_text":"","edits":[{"old_text":"one","new_text":"ONE"},{"old_text":"three","new_text":"THREE"}]}"#,
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\nTHREE\n");
}

#[tokio::test]
async fn whitespace_quotes_and_dashes_match_fuzzily() {
    let (root, path) = sample("const msg = \u{201c}hello\u{201d}  \nconst dash = \"a\u{2014}b\"\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","old_text":"const msg = \"hello\"\nconst dash = \"a-b\"","new_text":"const msg = \"hi\"\nconst dash = \"a-b\""}"#,
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "const msg = \u{201c}hi\u{201d}  \nconst dash = \"a\u{2014}b\"\n"
    );
}

#[tokio::test]
async fn a_fuzzy_match_rewrites_only_the_changed_span() {
    for (name, file, old_text, new_text, want) in [
        (
            "keeps markdown hard break",
            "line one  \nline two\n",
            "line one\nline two",
            "line one\nline 2",
            "line one  \nline 2\n",
        ),
        (
            "keeps curly quotes and em dash",
            "x = \u{201c}a\u{201d} \u{2014} b\n",
            "x = \"a\" - b",
            "x = \"a\" - c",
            "x = \u{201c}a\u{201d} \u{2014} c\n",
        ),
        (
            "keeps trailing whitespace after changed word",
            "foo  \nbar\n",
            "foo\nbar",
            "baz\nbar",
            "baz  \nbar\n",
        ),
        (
            "keeps next line indentation",
            "foo  \n    bar\n",
            "foo\n    ",
            "FOO\n    ",
            "FOO  \n    bar\n",
        ),
        (
            "keeps old_text trailing whitespace that new_text makes interior",
            "say(\u{201c}hi\u{201d})\n",
            "say(\"hi\") ",
            "say(\"hi\") // x",
            "say(\u{201c}hi\u{201d}) // x\n",
        ),
        (
            "inserts before the file's trailing whitespace",
            "say(\u{201c}hi\u{201d})  \n",
            "say(\"hi\") ",
            "say(\"hi\") // x",
            "say(\u{201c}hi\u{201d}) // x  \n",
        ),
        (
            "inserts between fuzzy lines",
            "a  \nb\n",
            "a\nb",
            "a\nX\nb",
            "a  \nX\nb\n",
        ),
    ] {
        let (root, path) = sample(file);
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": old_text,
            "new_text": new_text,
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        assert!(!result.is_error, "{name}: {result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want, "{name}");
    }
}

#[tokio::test]
async fn whitespace_only_old_text_is_rejected() {
    let (root, path) = sample("abc  \n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"\t\n","new_text":"X"}"#,
    )
    .await;
    assert!(
        result.is_error && result.content.contains("old_text was not found"),
        "{result:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc  \n");
}

/// Stripping the byte-order mark leaves an empty needle, which once
/// panicked while counting occurrences.
#[tokio::test]
async fn a_bom_only_old_text_is_rejected() {
    let (root, path) = sample("\u{feff}hello\n");
    let workspace = workspace(root.path());
    let arguments = serde_json::json!({
        "path": "sample.txt",
        "old_text": "\u{feff}",
        "new_text": "X",
    })
    .to_string();
    let result = run(&EditTool::new(&workspace), &arguments).await;
    assert!(
        result.is_error && result.content.contains("old_text was not found"),
        "{result:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}hello\n");
}

/// Trailing whitespace of old_text is ignored only where a file line ends:
/// mid-line it must not match other text and lose new_text's whitespace.
#[tokio::test]
async fn trailing_old_text_whitespace_matches_only_at_a_line_end() {
    for (name, file, old_text, new_text, want) in [
        (
            "mid-line tab is not ignored",
            "x = foo_bar \u{2014} 1\n",
            "foo\t",
            "baz\t",
            None,
        ),
        (
            "line-end whitespace still matches",
            "say \u{201c}hi\u{201d}\nnext\n",
            "say \"hi\"  ",
            "say \"bye\"  ",
            Some("say \u{201c}bye\u{201d}\nnext\n"),
        ),
        (
            "only line-end candidates count toward ambiguity",
            "a \u{2014} b\na \u{2014} bc\n",
            "a - b ",
            "a - B ",
            Some("a \u{2014} B\na \u{2014} bc\n"),
        ),
    ] {
        let (root, path) = sample(file);
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": old_text,
            "new_text": new_text,
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        match want {
            Some(want) => {
                assert!(!result.is_error, "{name}: {result:?}");
                assert_eq!(std::fs::read_to_string(&path).unwrap(), want, "{name}");
            }
            None => {
                assert!(
                    result.is_error && result.content.contains("old_text was not found"),
                    "{name}: {result:?}"
                );
                assert_eq!(std::fs::read_to_string(&path).unwrap(), file, "{name}");
            }
        }
    }
}

#[tokio::test]
async fn a_bom_prefixed_old_text_matches() {
    for new_text in ["bye", "\u{feff}bye"] {
        let (root, path) = sample("\u{feff}hello\nworld\n");
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": "sample.txt",
            "old_text": "\u{feff}hello",
            "new_text": new_text,
        })
        .to_string();
        let result = run(&EditTool::new(&workspace), &arguments).await;
        assert!(!result.is_error, "new_text {new_text:?}: {result:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "\u{feff}bye\nworld\n",
            "new_text {new_text:?}"
        );
    }
}

#[tokio::test]
async fn a_stray_cr_does_not_change_the_detected_newline() {
    let (root, path) = sample("a\rb\nc\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"c","new_text":"x\ny"}"#,
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\rb\nx\ny\n");
}

#[tokio::test]
async fn the_diff_renders_one_hunk_per_edit() {
    let lines: Vec<String> = (1..=200).map(|index| format!("line {index}")).collect();
    let (root, _) = sample(&format!("{}\n", lines.join("\n")));
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"line 5\n","new_text":"line 5\nextra\n"},{"old_text":"line 200\n","new_text":"LAST\n"}]}"#,
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    for want in [
        "@@ -3,6 +3,7 @@",
        "+extra",
        "@@ -197,5 +198,5 @@",
        "-line 200",
        "+LAST",
    ] {
        assert!(
            result.content.contains(want),
            "diff missing {want:?}: {result:?}"
        );
    }
    for absent in ["line 100", "truncated", "-line 6"] {
        assert!(
            !result.content.contains(absent),
            "diff includes {absent:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn the_diff_merges_edits_on_one_line() {
    let (root, _) = sample("a b c\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"A"},{"old_text":"c","new_text":"C"}]}"#,
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.content.matches("-a b c").count(), 1, "{result:?}");
    assert_eq!(result.content.matches("+A b C").count(), 1, "{result:?}");
    assert_eq!(result.content.matches("@@").count(), 2, "{result:?}");
}

#[tokio::test]
async fn write_and_edit_share_the_file_lock() {
    let (root, path) = sample("a\n");
    let workspace = workspace(root.path());
    let key = workspace.write_relative(Path::new("sample.txt")).unwrap();
    let edit = EditTool::new(&workspace);
    let write = crate::tool::write::WriteTool::new(&workspace);
    let calls: [(&str, &dyn Tool, &str); 2] = [
        ("write", &write, r#"{"path":"sample.txt","content":"b\n"}"#),
        (
            "edit",
            &edit,
            r#"{"path":"sample.txt","old_text":"a","new_text":"b"}"#,
        ),
    ];
    for (name, tool, arguments) in calls {
        let guard = workspace.lock_path(&key).await;
        let mut call = Box::pin(run(tool, arguments));
        let blocked = tokio::time::timeout(std::time::Duration::from_millis(50), &mut call).await;
        assert!(
            blocked.is_err(),
            "{name} completed while the file lock was held: {blocked:?}"
        );
        drop(guard);
        let result = call.await;
        assert!(!result.is_error, "{name}: {result:?}");
        std::fs::write(&path, "a\n").unwrap();
    }
}

#[tokio::test]
async fn a_byte_order_mark_and_crlf_endings_survive() {
    let (root, path) = sample("\u{feff}a\r\nb\r\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"a\nb\n","new_text":"x\ny\n"}"#,
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "\u{feff}x\r\ny\r\n"
    );
}

#[tokio::test]
async fn overlapping_edits_are_refused_without_touching_the_file() {
    let (root, path) = sample("abcdef\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"abc","new_text":"ABC"},{"old_text":"bcd","new_text":"BCD"}]}"#,
        )
        .await;
    assert!(
        result.is_error && result.content.contains("overlap"),
        "{result:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "abcdef\n");
}

#[tokio::test]
async fn every_edit_matches_against_the_original_content() {
    let (root, path) = sample("a\n");
    let workspace = workspace(root.path());
    let result = run(
            &EditTool::new(&workspace),
            r#"{"path":"sample.txt","edits":[{"old_text":"a","new_text":"b"},{"old_text":"b","new_text":"c"}]}"#,
        )
        .await;
    assert!(
        result.is_error && result.content.contains("old_text was not found"),
        "{result:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
}

#[tokio::test]
async fn the_diff_is_compact_and_trims_unchanged_lines() {
    let lines: Vec<String> = (1..=40).map(|index| format!("line {index}")).collect();
    let (root, _) = sample(&format!("{}\n", lines.join("\n")));
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"sample.txt","old_text":"line 20\n","new_text":"changed 20\n"}"#,
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    for want in [
        "@@ -17,7 +17,7 @@",
        "-line 20",
        "+changed 20",
        " line 17",
        " line 23",
    ] {
        assert!(
            result.content.contains(want),
            "diff missing {want:?}: {result:?}"
        );
    }
    for absent in ["line 13", "line 27", "line 40"] {
        assert!(
            !result.content.contains(absent),
            "diff includes {absent:?}: {result:?}"
        );
    }

    let (trimmed_root, _) = sample("alpha\nbeta\ngamma\n");
    let trimmed_workspace = crate::tool::testutil::workspace(trimmed_root.path());
    let trimmed = run(
            &EditTool::new(&trimmed_workspace),
            r#"{"path":"sample.txt","old_text":"alpha\nbeta\ngamma\n","new_text":"alpha\nCHANGED\ngamma\n"}"#,
        )
        .await;
    assert!(!trimmed.is_error, "{trimmed:?}");
    assert!(
        trimmed.content.contains("-beta") && trimmed.content.contains("+CHANGED"),
        "{trimmed:?}"
    );
    assert!(
        !trimmed.content.contains("-alpha") && !trimmed.content.contains("-gamma"),
        "{trimmed:?}"
    );
}

#[tokio::test]
async fn an_oversized_diff_is_truncated_but_the_file_is_written_whole() {
    let (root, path) = sample("target\n");
    let workspace = workspace(root.path());
    let replacement = "replacement line\n".repeat(2000);
    let arguments = serde_json::json!({
        "path": "sample.txt",
        "old_text": "target\n",
        "new_text": replacement,
    })
    .to_string();
    let result = run(&EditTool::new(&workspace), &arguments).await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("truncated"), "{result:?}");
    assert!(
        result.content.len() <= 8192,
        "len = {}",
        result.content.len()
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().len() as usize,
        replacement.len()
    );
}

#[tokio::test]
async fn traversal_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("inner");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(root.path().join("escape.txt"), "x").unwrap();
    let workspace = workspace(&nested);
    let result = run(
        &EditTool::new(&workspace),
        r#"{"path":"../escape.txt","old_text":"x","new_text":"y"}"#,
    )
    .await;
    assert!(
        result.is_error && result.content.contains("escapes workspace"),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_internal_final_symlink_is_edited_through() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target.txt");
    std::fs::write(&target, "write").unwrap();
    std::os::unix::fs::symlink("target.txt", root.path().join("relative-link")).unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("absolute-link")).unwrap();
    let workspace = workspace(root.path());
    let tool = EditTool::new(&workspace);

    for (link, old, new) in [
        ("relative-link", "write", "edit"),
        ("absolute-link", "edit", "edited"),
    ] {
        let result = run(
            &tool,
            &format!(r#"{{"path":"{link}","old_text":"{old}","new_text":"{new}"}}"#),
        )
        .await;
        assert!(!result.is_error, "edit({link}) = {result:?}");
        assert!(
            std::fs::symlink_metadata(root.path().join(link))
                .unwrap()
                .file_type()
                .is_symlink(),
            "link {link} was replaced"
        );
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn text(rng: &mut Rng, alphabet: &[&str], max: usize) -> String {
    let len = rng.below(max + 1);
    (0..len).map(|_| rng.pick(alphabet)).collect()
}

fn count(haystack: &str, needle: &str) -> usize {
    occurrences(haystack, needle, |_| true).1
}

fn ascii(text: &str) -> String {
    normalize_for_fuzzy_match_with_map(text).0
}

fn edit(old: &str, new: &str) -> Vec<EditReplacement> {
    vec![EditReplacement {
        old_text: old.to_owned(),
        new_text: new.to_owned(),
    }]
}

fn sub(rng: &mut Rng, content: &str) -> Option<String> {
    let bounds: Vec<usize> = (0..=content.len())
        .filter(|i| content.is_char_boundary(*i))
        .collect();
    if bounds.len() < 2 {
        return None;
    }
    let a = rng.below(bounds.len() - 1);
    let b = a + 1 + rng.below(bounds.len() - 1 - a);
    Some(content[bounds[a]..bounds[b]].to_owned())
}

/// A unique exact match equals `replacen` on the content; any other count is an error.
#[test]
fn random_exact_edits_match_a_reference_replacement() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let file_chars = ["a", "b", "\n", " ", "\t", "\u{2019}", "x"];
    let new_chars = ["a", "c", "\n", " ", "\u{2014}"];
    let mut checked = 0;
    for _ in 0..300_000 {
        let content = text(&mut rng, &file_chars, 60);
        let nl = if rng.below(2) == 0 || !content.contains('\n') {
            "\n"
        } else {
            "\r\n"
        };
        let bom = rng.below(4) == 0;
        let body = content.replace('\n', nl);
        let file = if bom { format!("\u{feff}{body}") } else { body };
        let Some(old) = sub(&mut rng, &content) else {
            continue;
        };
        let new = text(&mut rng, &new_chars, 16);
        let result = apply_text_edits(&file, "f", &edit(&old, &new));
        match count(&content, &old) {
            1 => {
                let expected = content.replacen(&old, &new, 1).replace('\n', nl);
                let expected = if bom {
                    format!("\u{feff}{expected}")
                } else {
                    expected
                };
                let got = result.expect("unique exact match applies").0;
                assert_eq!(got, expected, "file={file:?} old={old:?} new={new:?}");
                checked += 1;
            }
            _ => assert!(result.is_err(), "file={file:?} old={old:?}"),
        }
    }
    assert!(
        checked >= 200_000,
        "only {checked} exact cases were checked"
    );
}

/// A fuzzy match changes only the matched lines, ignoring the whitespace the fuzzy view drops.
#[test]
fn random_fuzzy_edits_change_only_the_matched_lines() {
    let mut rng = Rng(0x0fed_cba9_8765_4321);
    let file_chars = [
        "a", "b", "\n", " ", " ", "\u{2019}", "\u{201c}", "\u{2014}", "\u{00a0}", "'", "-",
    ];
    let new_chars = ["a", "c", "\n", "'", "-", "\u{2019}"];
    let mut checked = 0;
    for _ in 0..300_000 {
        let content = text(&mut rng, &file_chars, 60);
        let nl = if rng.below(2) == 0 || !content.contains('\n') {
            "\n"
        } else {
            "\r\n"
        };
        let file = content.replace('\n', nl);
        let Some(raw_old) = sub(&mut rng, &content) else {
            continue;
        };
        let old = ascii(&raw_old);
        if old.trim().is_empty() || count(&content, &old) != 0 {
            continue;
        }
        let new = text(&mut rng, &new_chars, 16);
        let Ok((got, _, _)) = apply_text_edits(&file, "f", &edit(&old, &new)) else {
            continue;
        };
        // Line endings stay in the file's style.
        if nl == "\r\n" {
            assert!(
                !got.replace("\r\n", "").contains('\n'),
                "file={file:?} old={old:?} new={new:?} got={got:?}"
            );
        }
        let got_lf = got.replace("\r\n", "\n");
        // Up to the fuzzy folding, the result is the file with the match replaced by new_text.
        let fuzzy = ascii(&content);
        let expected = ascii(&fuzzy.replacen(&ascii(&old), &new, 1));
        // Whitespace the fuzzy view dropped from the file stays in place
        // while new_text may add or drop spaces, so compare without spaces.
        let visible = |text: &str| text.chars().filter(|c| *c != ' ').collect::<String>();
        assert_eq!(
            visible(&ascii(&got_lf)),
            visible(&expected),
            "file={file:?} old={old:?} new={new:?} got={got_lf:?}"
        );
        // Lines outside the matched line range keep their exact bytes. An
        // unchanged result has nothing to compare and is skipped first.
        if got_lf == content {
            checked += 1;
            continue;
        }
        let match_at = fuzzy.find(&ascii(&old)).unwrap();
        let match_first = fuzzy[..match_at].matches('\n').count();
        let match_last = fuzzy[..match_at + ascii(&old).len()].matches('\n').count();
        let before: Vec<&str> = content.split('\n').collect();
        let after: Vec<&str> = got_lf.split('\n').collect();
        let tail = before.len() - match_last - 1;
        assert!(
            before[..match_first] == after[..match_first]
                && after.len() >= tail
                && before[match_last + 1..] == after[after.len() - tail..],
            "change outside the matched lines: file={file:?} old={old:?} new={new:?} got={got_lf:?}"
        );
        checked += 1;
    }
    assert!(
        checked >= 150_000,
        "only {checked} fuzzy cases were checked"
    );
}
