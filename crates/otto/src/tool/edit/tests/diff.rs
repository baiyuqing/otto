//! Tests for diff rendering, locking, overlap and file preservation.

use super::*;

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
