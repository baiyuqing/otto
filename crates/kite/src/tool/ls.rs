//! The `ls` tool.
//!
//! Lists one level of a workspace directory. Entries are sorted by name; a
//! symbolic link is suffixed with `@` and a directory with `/`, and the link is
//! never followed, so the listing cannot reveal a target outside the workspace.

use std::path::Path;

use kite_core::model::ToolDefinition;
use kite_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::{capped_text_result, decode_strict_json};
use super::workspace::Workspace;
use super::{CONTEXT_CANCELED, Tool, definition, error_result};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LsArgs {
    #[serde(default)]
    path: String,
}

/// Lists a workspace directory.
pub struct LsTool<'a> {
    workspace: &'a Workspace,
    max_output_bytes: usize,
}

impl<'a> LsTool<'a> {
    pub fn new(workspace: &'a Workspace, max_output_bytes: usize) -> Self {
        Self {
            workspace,
            max_output_bytes,
        }
    }
}

/// The schema advertised for `ls`.
pub fn ls_definition() -> ToolDefinition {
    definition(
        "ls",
        "List one level of a workspace directory (read-only)",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative directory to list; defaults to ."
                }
            }
        }),
    )
}

#[async_trait::async_trait]
impl Tool for LsTool<'_> {
    fn definition(&self) -> ToolDefinition {
        ls_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: LsArgs = match decode_strict_json(arguments.get(), &[]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let requested = if args.path.is_empty() {
            "."
        } else {
            args.path.as_str()
        };
        let directory = match self.workspace.existing_relative(Path::new(requested)) {
            Ok(directory) => directory,
            Err(error) => return error_result(error),
        };
        let file = match self.workspace.open_relative(&directory) {
            Ok(file) => file,
            Err(error) => return error_result(error),
        };
        match file.metadata() {
            Ok(metadata) if !metadata.is_dir() => {
                return error_result(format!("not a directory: {}", args.path));
            }
            Ok(_) => {}
            Err(error) => return error_result(error),
        }
        drop(file);
        let entries = match self.workspace.root_fs().read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => return error_result(error),
        };
        let mut output = String::new();
        for entry in entries {
            if cancel.is_cancelled() {
                return error_result(CONTEXT_CANCELED);
            }
            output.push_str(&entry.name.to_string_lossy());
            if entry.is_symlink {
                output.push('@');
            } else if entry.is_dir {
                output.push('/');
            }
            output.push('\n');
        }
        capped_text_result(&output, self.max_output_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::{MAX_OUTPUT_BYTES, run, run_cancelled, workspace};

    fn write_file(root: &std::path::Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn one_level_is_listed_sorted_with_type_suffixes_and_dotfiles() {
        let root = tempfile::tempdir().unwrap();
        write_file(root.path(), "a.txt", "a");
        write_file(root.path(), ".hidden", "hidden");
        write_file(root.path(), "dir/nested.txt", "nested");
        std::os::unix::fs::symlink(root.path().join("a.txt"), root.path().join("link")).unwrap();
        let workspace = workspace(root.path());
        let tool = LsTool::new(&workspace, MAX_OUTPUT_BYTES);

        let listing = run(&tool, "{}").await;
        assert!(!listing.is_error, "{listing:?}");
        assert_eq!(listing.content, ".hidden\na.txt\ndir/\nlink@\n");

        let scoped = run(&tool, r#"{"path":"dir"}"#).await;
        assert!(!scoped.is_error, "{scoped:?}");
        assert_eq!(scoped.content, "nested.txt\n");
    }

    #[tokio::test]
    async fn strict_arguments_cancellation_and_the_workspace_boundary_are_enforced() {
        let root = tempfile::tempdir().unwrap();
        write_file(root.path(), "file.txt", "x");
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();
        let workspace = workspace(root.path());
        let tool = LsTool::new(&workspace, MAX_OUTPUT_BYTES);

        for arguments in [
            r#"{"extra":true}"#,
            r#"{"path":"missing"}"#,
            r#"{"path":"file.txt"}"#,
            r#"{"path":".."}"#,
            r#"{"path":"outside"}"#,
        ] {
            let result = run(&tool, arguments).await;
            assert!(
                result.is_error,
                "ls({arguments}) = {result:?}, want an error"
            );
        }

        let cancelled = run_cancelled(&tool, "{}").await;
        assert!(
            cancelled.is_error && cancelled.content.contains(crate::tool::CONTEXT_CANCELED),
            "{cancelled:?}"
        );
    }

    #[tokio::test]
    async fn output_is_capped_with_a_valid_truncation_marker() {
        let root = tempfile::tempdir().unwrap();
        write_file(root.path(), "long-file-name.txt", "x");
        let workspace = workspace(root.path());
        let result = run(&LsTool::new(&workspace, 8), "{}").await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("truncated"), "{result:?}");
        assert!(result.content.len() <= 80, "{result:?}");
    }
}
