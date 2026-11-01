//! Tests for tool name and input translation between Otto and Claude Code.
use super::tool_names::{ClaudeTool, to_claude, to_otto};
use serde_json::{Value, json};
use std::path::Path;

const WORKSPACE: &str = "/ws";

fn forward(otto_tool: &str, arguments: Value) -> ClaudeTool {
    to_claude(otto_tool, &arguments, Path::new(WORKSPACE))
}

fn reverse(otto_tool: &str, original: Value, updated: Value) -> Value {
    to_otto(otto_tool, &original, &updated, Path::new(WORKSPACE)).unwrap()
}

#[test]
fn bash_keeps_fields() {
    let tool = forward("bash", json!({"command": "ls", "timeout": 5}));
    assert_eq!(tool.name, "Bash");
    assert_eq!(tool.input, json!({"command": "ls", "timeout": 5}));
    assert_eq!(
        reverse("bash", json!({}), json!({"command": "pwd", "timeout": 5})),
        json!({"command": "pwd", "timeout": 5})
    );
}

#[test]
fn read_renames_path_to_absolute_file_path() {
    let tool = forward("read", json!({"path": "src/a.rs", "offset": 3}));
    assert_eq!(tool.name, "Read");
    assert_eq!(
        tool.input,
        json!({"file_path": "/ws/src/a.rs", "offset": 3})
    );
    assert_eq!(
        reverse(
            "read",
            json!({}),
            json!({"file_path": "/ws/src/a.rs", "offset": 3})
        ),
        json!({"path": "src/a.rs", "offset": 3})
    );
}

#[test]
fn write_renames_path_to_absolute_file_path() {
    let tool = forward("write", json!({"path": "out.txt", "content": "x"}));
    assert_eq!(tool.name, "Write");
    assert_eq!(
        tool.input,
        json!({"file_path": "/ws/out.txt", "content": "x"})
    );
    assert_eq!(
        reverse(
            "write",
            json!({}),
            json!({"file_path": "/ws/out.txt", "content": "x"})
        ),
        json!({"path": "out.txt", "content": "x"})
    );
}

#[test]
fn edit_renames_path_and_text_fields() {
    let tool = forward(
        "edit",
        json!({"path": "a.rs", "old_text": "o", "new_text": "n"}),
    );
    assert_eq!(tool.name, "Edit");
    assert_eq!(
        tool.input,
        json!({"file_path": "/ws/a.rs", "old_string": "o", "new_string": "n"})
    );
    assert_eq!(
        reverse(
            "edit",
            json!({}),
            json!({"file_path": "/ws/a.rs", "old_string": "o", "new_string": "n"})
        ),
        json!({"path": "a.rs", "old_text": "o", "new_text": "n"})
    );
}

#[test]
fn edit_edits_array_items_are_renamed_both_ways() {
    let otto = json!({"path": "a.rs", "edits": [{"old_text": "a", "new_text": "b"}, {"old_text": "c", "new_text": "d"}]});
    let claude = json!({
        "file_path": "/ws/a.rs",
        "edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}]
    });
    assert_eq!(forward("edit", otto.clone()).input, claude);
    assert_eq!(reverse("edit", json!({}), claude), otto);
}

#[test]
fn grep_renames_ignore_case_and_absolutizes_path() {
    let tool = forward(
        "grep",
        json!({"pattern": "x", "ignore_case": true, "path": "src"}),
    );
    assert_eq!(tool.name, "Grep");
    assert_eq!(
        tool.input,
        json!({"pattern": "x", "-i": true, "path": "/ws/src"})
    );
    assert_eq!(
        reverse(
            "grep",
            json!({}),
            json!({"pattern": "x", "-i": true, "path": "/ws/src"})
        ),
        json!({"pattern": "x", "ignore_case": true, "path": "src"})
    );
}

#[test]
fn find_maps_to_glob_with_absolute_path() {
    let tool = forward("find", json!({"pattern": "*.rs", "path": "crates"}));
    assert_eq!(tool.name, "Glob");
    assert_eq!(tool.input, json!({"pattern": "*.rs", "path": "/ws/crates"}));
    assert_eq!(
        reverse(
            "find",
            json!({}),
            json!({"pattern": "*.rs", "path": "/ws/crates"})
        ),
        json!({"pattern": "*.rs", "path": "crates"})
    );
}

#[test]
fn absolute_path_stays_unchanged_going_to_claude() {
    let tool = forward("read", json!({"path": "/etc/hosts"}));
    assert_eq!(tool.input, json!({"file_path": "/etc/hosts"}));
}

#[test]
fn workspace_path_reverses_to_dot() {
    assert_eq!(
        reverse("read", json!({}), json!({"file_path": "/ws"})),
        json!({"path": "."})
    );
    assert_eq!(
        reverse("grep", json!({}), json!({"path": "/ws"})),
        json!({"path": "."})
    );
}

#[test]
fn absolute_path_outside_workspace_reverses_unchanged() {
    assert_eq!(
        reverse("read", json!({}), json!({"file_path": "/etc/hosts"})),
        json!({"path": "/etc/hosts"})
    );
    assert_eq!(
        reverse("read", json!({}), json!({"file_path": "/wsx/a"})),
        json!({"path": "/wsx/a"})
    );
}

#[test]
fn relative_path_reverses_unchanged() {
    assert_eq!(
        reverse("read", json!({}), json!({"file_path": "a.rs"})),
        json!({"path": "a.rs"})
    );
}

#[test]
fn mcp_call_tool_unwraps_name_and_arguments() {
    let tool = forward(
        "mcp_call_tool",
        json!({"tool": "mcp__demo__read", "arguments": {"id": 1}}),
    );
    assert_eq!(tool.name, "mcp__demo__read");
    assert_eq!(tool.input, json!({"id": 1}));
}

#[test]
fn mcp_call_tool_without_inner_arguments_has_empty_input() {
    let tool = forward("mcp_call_tool", json!({"tool": "mcp__demo__read"}));
    assert_eq!(tool.input, json!({}));
}

#[test]
fn mcp_call_tool_reverse_wraps_input_and_keeps_other_fields() {
    let original = json!({"tool": "mcp__demo__read", "arguments": {"id": 1}, "extra": "keep"});
    let updated = reverse("mcp_call_tool", original, json!({"id": 2}));
    assert_eq!(
        updated,
        json!({"tool": "mcp__demo__read", "arguments": {"id": 2}, "extra": "keep"})
    );
}

#[test]
fn non_object_updated_input_is_an_error() {
    for tool in ["bash", "read", "mcp_call_tool", "skill"] {
        for updated in [json!("text"), json!(null), json!([1]), json!(3)] {
            let result = to_otto(tool, &json!({}), &updated, Path::new(WORKSPACE));
            assert!(result.is_err(), "{tool} accepted {updated}");
        }
    }
}

#[test]
fn unmapped_tool_passes_both_ways_unchanged() {
    let arguments = json!({"name": "demo", "path": "rel"});
    let tool = forward("skill", arguments.clone());
    assert_eq!(tool.name, "skill");
    assert_eq!(tool.input, arguments);
    assert_eq!(reverse("skill", json!({}), arguments.clone()), arguments);
}

#[test]
fn non_object_arguments_do_not_panic() {
    for arguments in [json!(null), json!("text"), json!([1, 2]), json!(7)] {
        for tool in [
            "bash",
            "read",
            "write",
            "edit",
            "grep",
            "find",
            "mcp_call_tool",
            "skill",
        ] {
            let _ = forward(tool, arguments.clone());
        }
    }
    let tool = forward("read", json!("text"));
    assert_eq!(tool.name, "Read");
    assert_eq!(tool.input, json!("text"));
    assert_eq!(
        forward("mcp_call_tool", json!("text")).name,
        "mcp_call_tool"
    );
}

#[test]
fn mcp_call_tool_reverse_with_non_object_original_creates_object() {
    let updated = reverse("mcp_call_tool", json!(null), json!({"id": 2}));
    assert_eq!(updated, json!({"arguments": {"id": 2}}));
}
