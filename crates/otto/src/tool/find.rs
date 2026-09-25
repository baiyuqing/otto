//! The `find` tool.
//!
//! Lists workspace files whose path matches a relative glob. Symbolic links are
//! reported but never followed, `.git` subtrees are skipped, and a search root
//! that resolves into repository metadata returns nothing.

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::gitignore::GitignoreStack;
use super::result::{capped_text_result, decode_strict_json};
use super::search::{
    WalkAction, match_glob_segments, resolve_search_limit, search_relative_path,
    search_root_inside_git, validated_glob_segments, walk_dir_ignoring,
};
use super::workspace::Workspace;
use super::{CONTEXT_CANCELED, Tool, definition, error_result};

const DEFAULT_FIND_LIMIT: usize = 1000;
const MAXIMUM_FIND_LIMIT: usize = 10000;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    no_ignore: bool,
}

/// Finds workspace files by glob.
pub struct FindTool<'a> {
    workspace: &'a Workspace,
    max_output_bytes: usize,
}

impl<'a> FindTool<'a> {
    pub fn new(workspace: &'a Workspace, max_output_bytes: usize) -> Self {
        Self {
            workspace,
            max_output_bytes,
        }
    }
}

/// The schema advertised for `find`.
pub fn find_definition() -> ToolDefinition {
    definition(
        "find",
        "Find workspace files by glob pattern (read-only). Files the workspace's .gitignore excludes are skipped unless no_ignore is set",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Relative glob pattern; supports recursive ** segments"
                },
                "path": {
                    "type": "string",
                    "description": "Workspace-relative directory or file to search; defaults to ."
                },
                "no_ignore": {
                    "type": "boolean",
                    "description": "Search files the repository's .gitignore excludes; defaults to false"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAXIMUM_FIND_LIMIT,
                    "description": "Maximum matching files; defaults to 1000"
                }
            },
            "required": ["pattern"]
        }),
    )
}

#[async_trait::async_trait]
impl Tool for FindTool<'_> {
    fn definition(&self) -> ToolDefinition {
        find_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: FindArgs = match decode_strict_json(arguments.get(), &["pattern"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if args.pattern.is_empty() {
            return error_result("pattern must not be empty");
        }
        let glob_segments = match validated_glob_segments(&args.pattern) {
            Ok(segments) => segments,
            Err(message) => return error_result(message),
        };
        let limit = match resolve_search_limit(args.limit, DEFAULT_FIND_LIMIT, MAXIMUM_FIND_LIMIT) {
            Ok(limit) => limit,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let search_path = if args.path.is_empty() {
            "."
        } else {
            &args.path
        };
        let root = match self
            .workspace
            .existing_relative(std::path::Path::new(search_path))
        {
            Ok(root) => root.to_string_lossy().into_owned(),
            Err(error) => return error_result(error),
        };
        match search_root_inside_git(self.workspace, search_path, &root) {
            Ok(true) => return ToolResult::default(),
            Ok(false) => {}
            Err(error) => return error_result(error),
        }

        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut ignore =
            (!args.no_ignore).then(|| GitignoreStack::for_root(self.workspace.root_fs(), &root));
        let walk = walk_dir_ignoring(
            self.workspace.root_fs(),
            &root,
            ignore.as_mut(),
            &mut |entry| {
                if cancel.is_cancelled() {
                    return Err(std::io::Error::other(CONTEXT_CANCELED));
                }
                if entry.path != root && entry.name == ".git" {
                    return Ok(if entry.is_dir {
                        WalkAction::SkipDir
                    } else {
                        WalkAction::Continue
                    });
                }
                if entry.is_dir || entry.is_symlink || !entry.is_regular {
                    return Ok(WalkAction::Continue);
                }
                let candidate = search_relative_path(&root, &entry.path)?;
                if !match_glob_segments(&glob_segments, &candidate) {
                    return Ok(WalkAction::Continue);
                }
                if matches.len() >= limit {
                    truncated = true;
                    return Ok(WalkAction::Stop);
                }
                matches.push(entry.path.clone());
                Ok(WalkAction::Continue)
            },
        );
        if let Err(error) = walk {
            return error_result(error);
        }

        let mut output = String::new();
        for entry in &matches {
            output.push_str(entry);
            output.push('\n');
        }
        if truncated {
            output.push_str("[truncated: result limit reached]\n");
        }
        capped_text_result(&output, self.max_output_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::{
        MAX_OUTPUT_BYTES, run, run_cancelled, workspace, write_search_file,
    };

    #[tokio::test]
    async fn recursive_globs_match_deterministically_and_skip_git_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "main.go",
            "src/test_one.go",
            "src/a/test_two.go",
            "src/skip.txt",
            ".hidden/test.go",
            ".git/secret.go",
        ] {
            write_search_file(root.path(), name, "content\n");
        }
        let outside = tempfile::tempdir().unwrap();
        write_search_file(outside.path(), "outside.go", "outside\n");
        std::os::unix::fs::symlink(
            outside.path().join("outside.go"),
            root.path().join("linked.go"),
        )
        .unwrap();

        let workspace = workspace(root.path());
        let tool = FindTool::new(&workspace, MAX_OUTPUT_BYTES);

        let result = run(&tool, r#"{"pattern":"**/*.go"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            ".hidden/test.go\nmain.go\nsrc/a/test_two.go\nsrc/test_one.go\n"
        );

        let scoped = run(&tool, r#"{"pattern":"**/test*.go","path":"src"}"#).await;
        assert!(!scoped.is_error, "{scoped:?}");
        assert_eq!(scoped.content, "src/a/test_two.go\nsrc/test_one.go\n");
    }

    #[tokio::test]
    async fn arguments_limits_cancellation_and_the_workspace_boundary_are_enforced() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), "a.go", "a");
        write_search_file(root.path(), "b.go", "b");
        let workspace = workspace(root.path());
        let tool = FindTool::new(&workspace, MAX_OUTPUT_BYTES);

        let limited = run(&tool, r#"{"pattern":"**/*.go","limit":1}"#).await;
        assert!(!limited.is_error, "{limited:?}");
        assert!(limited.content.starts_with("a.go\n"), "{limited:?}");
        assert!(
            limited.content.contains("result limit reached"),
            "{limited:?}"
        );
        assert!(!limited.content.contains("b.go"), "{limited:?}");

        for arguments in [
            "{}",
            r#"{"pattern":"[broken"}"#,
            r#"{"pattern":"**","limit":0}"#,
            r#"{"pattern":"**","limit":-1}"#,
            r#"{"pattern":"**","limit":10001}"#,
            r#"{"pattern":"**","extra":true}"#,
            r#"{"pattern":"**","path":".."}"#,
        ] {
            let result = run(&tool, arguments).await;
            assert!(
                result.is_error,
                "find({arguments}) = {result:?}, want an error"
            );
        }

        let cancelled = run_cancelled(&tool, r#"{"pattern":"**"}"#).await;
        assert!(
            cancelled.is_error && cancelled.content.contains(CONTEXT_CANCELED),
            "{cancelled:?}"
        );
    }

    #[tokio::test]
    async fn gitignored_files_are_skipped_unless_no_ignore_is_set() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), ".gitignore", "target/\n*.tmp\n!keep.tmp\n");
        for name in [
            "main.go",
            "keep.tmp",
            "drop.tmp",
            "target/build.go",
            "src/nested.go",
            "src/.gitignore",
            "src/nested.tmp",
        ] {
            write_search_file(root.path(), name, "content\n");
        }
        write_search_file(root.path(), "src/.gitignore", "nested.go\n");
        let workspace = workspace(root.path());
        let tool = FindTool::new(&workspace, MAX_OUTPUT_BYTES);

        let ignored = run(&tool, r#"{"pattern":"**/*"}"#).await;
        assert!(!ignored.is_error, "{ignored:?}");
        assert_eq!(
            ignored.content,
            ".gitignore\nkeep.tmp\nmain.go\nsrc/.gitignore\n"
        );

        let everything = run(&tool, r#"{"pattern":"**/*.go","no_ignore":true}"#).await;
        assert!(!everything.is_error, "{everything:?}");
        assert_eq!(
            everything.content,
            "main.go\nsrc/nested.go\ntarget/build.go\n"
        );
    }

    #[tokio::test]
    async fn an_explicitly_requested_ignored_root_is_searched() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), ".gitignore", "target/\n");
        write_search_file(root.path(), "target/build.go", "content\n");
        write_search_file(root.path(), "target/deep/more.go", "content\n");
        let workspace = workspace(root.path());
        let tool = FindTool::new(&workspace, MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"pattern":"**/*.go","path":"target"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "target/build.go\ntarget/deep/more.go\n");
    }

    #[tokio::test]
    async fn a_scoped_search_honors_a_parent_gitignore() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), ".gitignore", "*.tmp\n");
        write_search_file(root.path(), "src/keep.go", "content\n");
        write_search_file(root.path(), "src/drop.tmp", "content\n");
        let workspace = workspace(root.path());
        let tool = FindTool::new(&workspace, MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"pattern":"**/*","path":"src"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "src/keep.go\n");
    }

    #[tokio::test]
    async fn a_root_inside_the_git_directory_returns_nothing() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), ".git/config", "secret");
        let workspace = workspace(root.path());
        let tool = FindTool::new(&workspace, MAX_OUTPUT_BYTES);
        for search_path in [".git", ".git/config"] {
            let result = run(
                &tool,
                &format!(r#"{{"pattern":"**","path":"{search_path}"}}"#),
            )
            .await;
            assert!(
                !result.is_error && result.content.is_empty(),
                "{search_path}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn output_is_capped_with_a_valid_truncation_marker() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), "long-file-name.go", "x");
        let workspace = workspace(root.path());
        let result = run(&FindTool::new(&workspace, 8), r#"{"pattern":"**/*.go"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("truncated"), "{result:?}");
        assert!(result.content.len() <= 80, "{result:?}");
    }
}
