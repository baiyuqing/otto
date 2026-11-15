//! Translation of tool names and arguments between Otto's tools and Claude Code's tool
//! shapes, in both directions. Paths become absolute (joined to the workspace) on the way to
//! Claude and workspace-relative on the way back; fields without a rename pass unchanged.
use serde_json::{Map, Value, json};
use std::path::Path;

/// A tool call in Claude Code's shape: `tool_name` and `tool_input`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeTool {
    pub name: String,
    pub input: Value,
}

/// Otto tool name, Claude tool name, field renames (Otto name, Claude name) and the
/// path field (Claude name) that is made absolute.
type ToolShape = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    Option<&'static str>,
);
const TOOLS: [ToolShape; 6] = [
    ("bash", "Bash", &[], None),
    ("read", "Read", &[("path", "file_path")], Some("file_path")),
    (
        "write",
        "Write",
        &[("path", "file_path")],
        Some("file_path"),
    ),
    (
        "edit",
        "Edit",
        &[
            ("path", "file_path"),
            ("old_text", "old_string"),
            ("new_text", "new_string"),
        ],
        Some("file_path"),
    ),
    ("grep", "Grep", &[("ignore_case", "-i")], Some("path")),
    ("find", "Glob", &[], Some("path")),
];
/// Otto's MCP dispatcher: `arguments.tool` is the `mcp__<server>__<tool>` name.
const MCP_DISPATCHER: &str = "mcp_call_tool";

fn rename_fields(input: &Value, renames: &[(&str, &str)], forward: bool) -> Value {
    let Some(object) = input.as_object() else {
        return input.clone();
    };
    let renamed: Map<String, Value> = object
        .iter()
        .map(|(key, value)| {
            let name = renames
                .iter()
                .find(|(otto, claude)| if forward { otto == key } else { claude == key })
                .map(|(otto, claude)| if forward { *claude } else { *otto })
                .unwrap_or(key.as_str());
            let value = if key == "edits" {
                match value.as_array() {
                    Some(items) => Value::Array(
                        items
                            .iter()
                            .map(|item| rename_fields(item, renames, forward))
                            .collect(),
                    ),
                    None => value.clone(),
                }
            } else {
                value.clone()
            };
            (name.to_owned(), value)
        })
        .collect();
    Value::Object(renamed)
}

fn absolutize(input: &mut Value, field: &str, workspace: &Path) {
    if let Some(path) = input.get(field).and_then(Value::as_str)
        && !Path::new(path).is_absolute()
    {
        let absolute = workspace.join(path).to_string_lossy().into_owned();
        input[field] = Value::String(absolute);
    }
}

fn relativize(input: &mut Value, field: &str, workspace: &Path) {
    if let Some(path) = input.get(field).and_then(Value::as_str) {
        let relative = match Path::new(path).strip_prefix(workspace) {
            Ok(rest) if rest.as_os_str().is_empty() => Some(".".to_owned()),
            Ok(rest) => Some(rest.to_string_lossy().into_owned()),
            Err(_) => None,
        };
        if let Some(relative) = relative {
            input[field] = Value::String(relative);
        }
    }
}

/// Claude's `tool_name` and `tool_input` for an Otto tool call. Unmapped tools keep their
/// name and arguments; `mcp_call_tool` unwraps to the MCP tool name and its arguments.
pub(crate) fn to_claude(otto_tool: &str, arguments: &Value, workspace: &Path) -> ClaudeTool {
    if otto_tool == MCP_DISPATCHER {
        let name = arguments
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or(MCP_DISPATCHER)
            .to_owned();
        let input = arguments
            .get("arguments")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        return ClaudeTool { name, input };
    }
    match TOOLS.iter().find(|(otto, ..)| *otto == otto_tool) {
        Some((_, claude, renames, path_field)) => {
            let mut input = rename_fields(arguments, renames, true);
            if let Some(field) = path_field {
                absolutize(&mut input, field, workspace);
            }
            ClaudeTool {
                name: (*claude).to_owned(),
                input,
            }
        }
        None => ClaudeTool {
            name: otto_tool.to_owned(),
            input: arguments.clone(),
        },
    }
}

/// Otto `arguments` for a Claude `updatedInput` on the same tool call. `updated_input` must
/// be an object. For `mcp_call_tool` it replaces `arguments.arguments` and keeps the other
/// fields of `original_arguments`.
pub(crate) fn to_otto(
    otto_tool: &str,
    original_arguments: &Value,
    updated_input: &Value,
    workspace: &Path,
) -> Result<Value, String> {
    if !updated_input.is_object() {
        return Err("hook updatedInput must be an object".into());
    }
    if otto_tool == MCP_DISPATCHER {
        let mut arguments = original_arguments.clone();
        if !arguments.is_object() {
            arguments = json!({});
        }
        arguments["arguments"] = updated_input.clone();
        return Ok(arguments);
    }
    match TOOLS.iter().find(|(otto, ..)| *otto == otto_tool) {
        Some((_, _, renames, path_field)) => {
            let mut arguments = rename_fields(updated_input, renames, false);
            if let Some(field) = path_field {
                let otto_field = renames
                    .iter()
                    .find(|(_, claude)| claude == field)
                    .map_or(*field, |(otto, _)| *otto);
                relativize(&mut arguments, otto_field, workspace);
            }
            Ok(arguments)
        }
        None => Ok(updated_input.clone()),
    }
}
