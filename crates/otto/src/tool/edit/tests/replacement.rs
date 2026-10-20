//! Tests for exact and fuzzy replacement, edit shapes and line breaks.

use super::*;

#[tokio::test]
async fn an_exact_single_match_is_replaced_and_diffed() {
    let (root, path) = sample("hello world\n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(Arc::clone(&workspace)),
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
        &EditTool::new(Arc::clone(&workspace)),
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
            &EditTool::new(Arc::clone(&workspace)),
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
            &EditTool::new(Arc::clone(&workspace)),
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
            &EditTool::new(Arc::clone(&workspace)),
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
    let tool = EditTool::new(Arc::clone(&workspace));

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
    let tool = EditTool::new(Arc::clone(&workspace));
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
            &EditTool::new(Arc::clone(&workspace)),
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
            &EditTool::new(Arc::clone(&workspace)),
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
            &EditTool::new(Arc::clone(&workspace)),
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
        let result = run(&EditTool::new(Arc::clone(&workspace)), &arguments).await;
        assert!(!result.is_error, "{name}: {result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want, "{name}");
    }
}

#[tokio::test]
async fn whitespace_only_old_text_is_rejected() {
    let (root, path) = sample("abc  \n");
    let workspace = workspace(root.path());
    let result = run(
        &EditTool::new(Arc::clone(&workspace)),
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
    let result = run(&EditTool::new(Arc::clone(&workspace)), &arguments).await;
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
        let result = run(&EditTool::new(Arc::clone(&workspace)), &arguments).await;
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
        let result = run(&EditTool::new(Arc::clone(&workspace)), &arguments).await;
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
        &EditTool::new(Arc::clone(&workspace)),
        r#"{"path":"sample.txt","old_text":"c","new_text":"x\ny"}"#,
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\rb\nx\ny\n");
}
