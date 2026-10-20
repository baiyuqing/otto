//! Tests for ambiguous, absent and bounded match errors.

use super::*;

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
