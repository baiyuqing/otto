//! The `write` tool.
//!
//! Creates or replaces a workspace file. The write is atomic: content goes to a
//! temporary file in the destination directory and is renamed into place
//! through the workspace root handle, so a reader never observes a partial file
//! and the destination is never resolved outside the workspace. An existing
//! file's permission bits are preserved; a new file gets `0644`.

use std::io::Read;
use std::path::Path;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use nix::sys::stat::Mode;

use super::gopath::{bytes, dir, path_from};
use super::result::decode_strict_json;
use super::root::is_dir;
use super::workspace::Workspace;
use super::{Tool, definition, error_result, preflight_error, text_result};
use crate::fsops::{PosixPublish, Publish, PublishMode, PublishOps, publish_file};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    #[serde(default)]
    path: String,
    #[serde(default)]
    content: String,
}

/// Avoid loading a large existing file just to show a preview of a write.
const MAX_DIFF_INPUT_BYTES: u64 = 1 << 20;
const MAX_DIFF_OUTPUT_BYTES: usize = 4096;
const DIFF_CONTEXT_LINES: usize = 3;

/// A compact diff between the complete old and new contents. The normal
/// `edit` tool reports its own patch; `write` has no known changed spans, so
/// common leading/trailing lines delimit its one replacement hunk.
fn replacement_diff(before: Option<&str>, after: &str) -> String {
    if before == Some(after) || (before.is_none() && after.is_empty()) {
        return "(no textual changes)".to_owned();
    }
    let old: Vec<&str> = before.unwrap_or("").split_inclusive('\n').collect();
    let new: Vec<&str> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_change_end = old.len() - suffix;
    let new_change_end = new.len() - suffix;
    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let old_end = (old_change_end + DIFF_CONTEXT_LINES).min(old.len());
    let new_end = (new_change_end + DIFF_CONTEXT_LINES).min(new.len());
    let old_start_line = if old.is_empty() { 0 } else { context_start + 1 };
    let new_start_line = if new.is_empty() { 0 } else { context_start + 1 };
    let mut diff = format!(
        "@@ -{old_start_line},{} +{new_start_line},{} @@\n",
        old_end - context_start,
        new_end - context_start
    );
    for (marker, lines) in [
        (' ', &old[context_start..prefix]),
        ('-', &old[prefix..old_change_end]),
        ('+', &new[prefix..new_change_end]),
        (' ', &new[new_change_end..new_end]),
    ] {
        for line in lines {
            diff.push(marker);
            diff.push_str(line.strip_suffix('\n').unwrap_or(line));
            diff.push('\n');
            if !line.ends_with('\n') {
                diff.push_str("\\ No newline at end of file\n");
            }
        }
    }
    let diff = diff.trim_end_matches('\n');
    if diff.len() <= MAX_DIFF_OUTPUT_BYTES {
        return diff.to_owned();
    }
    let cut = diff.as_bytes()[..MAX_DIFF_OUTPUT_BYTES]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap_or_else(|| {
            (0..=MAX_DIFF_OUTPUT_BYTES)
                .rev()
                .find(|index| diff.is_char_boundary(*index))
                .unwrap_or(0)
        });
    format!("{}\n... (diff truncated)", &diff[..cut])
}

fn previous_text(workspace: &Workspace, path: &Path) -> Result<Option<String>, &'static str> {
    let file = match workspace.open_relative(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("previous content could not be read"),
    };
    let metadata = file
        .metadata()
        .map_err(|_| "previous content could not be read")?;
    if !metadata.is_file() || metadata.len() > MAX_DIFF_INPUT_BYTES {
        return Err("previous content is not a small regular text file");
    }
    let mut bytes = Vec::new();
    file.take(MAX_DIFF_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "previous content could not be read")?;
    if bytes.len() as u64 > MAX_DIFF_INPUT_BYTES || bytes.contains(&0) {
        return Err("previous content is not a small regular text file");
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "previous content is not a small regular text file")
}

/// Writes a workspace file.
pub struct WriteTool<'a> {
    workspace: &'a Workspace,
    publish: &'a dyn PublishOps,
}

impl<'a> WriteTool<'a> {
    pub fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
            publish: &PosixPublish,
        }
    }

    #[cfg(test)]
    fn with_publish_ops(workspace: &'a Workspace, publish: &'a dyn PublishOps) -> Self {
        Self { workspace, publish }
    }
}

/// The schema advertised for `write`.
pub fn write_definition() -> ToolDefinition {
    definition(
        "write",
        "Create or replace a file in the workspace. A successful result includes a bounded text diff when the previous content is readable; otherwise it explains why the diff is unavailable.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path to write"
                },
                "content": {
                    "type": "string",
                    "description": "Full file content"
                }
            },
            "required": ["path", "content"]
        }),
    )
}

pub(crate) const WRITE_RETRY: (&str, otto_core::model::RetrySafety) =
    ("write", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for WriteTool<'_> {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        WRITE_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        write_definition()
    }

    async fn execute(&self, arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        let args: WriteArgs = match decode_strict_json(arguments.get(), &["path", "content"]) {
            Ok(args) => args,
            Err(message) => return preflight_error(message),
        };
        if args.path.is_empty() {
            return preflight_error("missing required argument: path");
        }
        let relative = match self.workspace.write_relative(Path::new(&args.path)) {
            Ok(relative) => relative,
            Err(error) => return preflight_error(error),
        };
        let _guard = self.workspace.lock_path(&relative).await;
        let before = previous_text(self.workspace, &relative);
        if let Err(error) = write_file_atomic(
            self.workspace,
            &relative,
            args.content.as_bytes(),
            self.publish,
        ) {
            return error.into_tool_result();
        }
        let summary = format!("wrote {} ({} bytes)", args.path, args.content.len());
        let detail = match before {
            Ok(text) if args.content.len() as u64 <= MAX_DIFF_INPUT_BYTES => {
                replacement_diff(text.as_deref(), &args.content)
            }
            Ok(_) => "(diff unavailable: new content is too large)".to_owned(),
            Err(reason) => format!("(diff unavailable: {reason})"),
        };
        text_result(format!("{summary}\n{detail}"))
    }
}

/// Writes `content` to the root-relative `path` through a temporary file and a
/// rename.
pub(crate) fn write_file_atomic(
    workspace: &Workspace,
    path: &Path,
    content: &[u8],
    publish: &dyn PublishOps,
) -> Result<(), AtomicWriteError> {
    let before_rename = |error: String| AtomicWriteError::BeforeRename(error);
    let root = workspace.root_fs();
    let directory = path_from(dir(bytes(path)));
    root.mkdir_all(&directory, Mode::from_bits_truncate(0o755))
        .map_err(|error| before_rename(error.to_string()))?;

    let mut mode = 0o644u32;
    match root.stat(path) {
        Ok(stat) => {
            if is_dir(&stat) {
                return Err(before_rename(format!(
                    "path is a directory: {}",
                    path.display()
                )));
            }
            mode = super::root::mode_bits(&stat) & 0o777;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(before_rename(error.to_string())),
    }

    let refuse_git = |resolved: &[std::ffi::OsString]| {
        super::workspace::refuse_git_read_only(resolved.iter().map(|part| part.as_os_str()), path)
    };
    let (parent, name) = root
        .open_parent(path, &refuse_git)
        .map_err(|error| before_rename(error.to_string()))?;
    let suffix =
        crate::auth::random_hex::<8>().map_err(|error| before_rename(error.to_string()))?;
    let temp_name = format!(".otto-{suffix}");
    // The publish fsyncs the directory after the rename: `fsync` on the
    // temporary file covers the file's data and metadata but not the
    // directory entry that now points the destination name at it. Without
    // it, a host crash after a reported success can revert the rename.
    publish_file(
        &parent,
        &Publish {
            name: &name,
            temp: &temp_name,
            permissions: mode,
            mode: PublishMode::Replace,
        },
        content,
        publish,
    )
    .map_err(|error| {
        if error.committed {
            AtomicWriteError::AfterRename(format!(
                "file replaced but directory sync failed: {}",
                error.source
            ))
        } else {
            before_rename(error.source.to_string())
        }
    })
}

/// A failed atomic write distinguishes failures after the destination rename,
/// where the visible effect can no longer be established by the caller.
#[derive(Debug)]
pub(crate) enum AtomicWriteError {
    BeforeRename(String),
    AfterRename(String),
}

impl AtomicWriteError {
    pub(crate) fn into_tool_result(self) -> ToolResult {
        match self {
            Self::BeforeRename(message) => error_result(message),
            Self::AfterRename(message) => error_result(message).transport_unknown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::faults::publish::FaultyPublishOps;
    use crate::faults::{Fault, FaultError, FaultPlan, Rule, Trigger};
    use crate::fsops::PublishStep;
    use crate::tool::testutil::{run, workspace};
    use otto_core::model::{EffectCertainty, OperationDisposition};
    use std::os::unix::fs::PermissionsExt;

    fn publish_fault(step: PublishStep) -> std::sync::Arc<FaultPlan<PublishStep>> {
        FaultPlan::new(vec![Rule::new(
            step,
            Trigger::Always,
            Fault::Before(FaultError::Os(libc::EIO)),
        )])
    }

    #[tokio::test]
    async fn successful_writes_report_the_change_made() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let tool = WriteTool::new(&workspace);

        let created = run(&tool, r#"{"path":"file.txt","content":"one\ntwo\n"}"#).await;
        assert!(!created.is_error, "{created:?}");
        assert!(created.content.contains("@@ -0,0 +1,2 @@"), "{created:?}");
        assert!(created.content.contains("+one\n+two"), "{created:?}");

        let replaced = run(&tool, r#"{"path":"file.txt","content":"one\nthree\n"}"#).await;
        assert!(!replaced.is_error, "{replaced:?}");
        assert!(replaced.content.contains("-two"), "{replaced:?}");
        assert!(replaced.content.contains("+three"), "{replaced:?}");
        assert!(!replaced.content.contains("+one"), "{replaced:?}");
    }

    #[test]
    fn a_write_diff_distinguishes_a_missing_final_newline() {
        let diff = replacement_diff(Some("same\n"), "same");
        assert!(diff.contains("-same\n+same"), "{diff}");
        assert!(diff.contains("\\ No newline at end of file"), "{diff}");
    }

    #[test]
    fn a_write_diff_truncates_long_unicode_text_on_a_character_boundary() {
        let diff = replacement_diff(None, &"你".repeat(2000));
        assert!(diff.contains("... (diff truncated)"), "{diff}");
        assert!(diff.len() < 4200, "{} bytes", diff.len());
    }

    #[tokio::test]
    async fn binary_previous_content_does_not_prevent_a_write() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.txt"), b"old\0value").unwrap();
        let workspace = workspace(root.path());

        let result = run(
            &WriteTool::new(&workspace),
            r#"{"path":"file.txt","content":"new"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("diff unavailable"), "{result:?}");
        assert_eq!(std::fs::read(root.path().join("file.txt")).unwrap(), b"new");
    }

    #[tokio::test]
    async fn git_config_and_hooks_are_refused_and_other_git_files_are_written() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git/hooks")).unwrap();
        std::fs::create_dir_all(root.path().join(".git/info")).unwrap();
        std::fs::write(root.path().join(".git/config"), "old").unwrap();
        std::os::unix::fs::symlink(".git", root.path().join("g")).unwrap();
        let workspace = workspace(root.path());
        let tool = WriteTool::new(&workspace);

        for path in [
            ".git/config",
            ".GIT/config",
            "g/config",
            ".git/hooks/pre-commit",
        ] {
            let result = run(&tool, &format!(r#"{{"path":"{path}","content":"new"}}"#)).await;
            assert!(
                result.is_error && result.content.contains("read-only"),
                "{path}: {result:?}"
            );
            assert_eq!(
                result.outcome_override.unwrap().effect_certainty,
                EffectCertainty::NotStarted
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join(".git/config")).unwrap(),
            "old"
        );
        assert!(!root.path().join(".git/hooks/pre-commit").exists());

        let ok = run(&tool, r#"{"path":".git/info/exclude","content":"x"}"#).await;
        assert!(!ok.is_error, "{ok:?}");
    }

    /// The check in `write_relative` runs before the write; a symlink created
    /// after it must still be refused, so the rename re-checks the path it
    /// resolves. Calling `write_file_atomic` directly skips the first check.
    #[test]
    fn the_atomic_write_refuses_git_paths_reached_through_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git/hooks")).unwrap();
        std::fs::write(root.path().join(".git/config"), "old").unwrap();
        std::os::unix::fs::symlink(".git", root.path().join("g")).unwrap();
        let workspace = workspace(root.path());

        for path in [
            "g/config",
            "g/hoo\u{212A}s/pre-commit",
            "g/hook\u{17F}/pre-commit",
        ] {
            let error = write_file_atomic(&workspace, Path::new(path), b"new", &PosixPublish)
                .expect_err(path);
            assert!(
                matches!(&error, AtomicWriteError::BeforeRename(message) if message.contains("read-only")),
                "{path}: {error:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join(".git/config")).unwrap(),
            "old"
        );
        assert!(!root.path().join(".git/hooks/pre-commit").exists());
        let leftovers: Vec<_> = walkdir(root.path().join(".git"))
            .into_iter()
            .filter(|name| name.contains(".otto-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    fn walkdir(directory: std::path::PathBuf) -> Vec<String> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                names.extend(walkdir(entry.path()));
            }
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names
    }

    #[tokio::test]
    async fn unknown_fields_and_a_missing_path_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let tool = WriteTool::new(&workspace);

        // Malformed JSON and trailing tokens are rejected by the stream
        // decoder before a `RawValue` exists; see `result::tests`.
        let unknown = run(&tool, r#"{"path":"file.txt","content":"x","extra":true}"#).await;
        assert!(
            unknown.is_error && unknown.content.contains("unknown field"),
            "{unknown:?}"
        );
        assert_eq!(
            unknown.outcome_override.unwrap().effect_certainty,
            EffectCertainty::NotStarted
        );

        let missing = run(&tool, r#"{"content":"hello"}"#).await;
        assert!(
            missing.is_error && missing.content.contains("path"),
            "{missing:?}"
        );
        assert_eq!(
            missing.outcome_override.unwrap().effect_certainty,
            EffectCertainty::NotStarted
        );
    }

    #[tokio::test]
    async fn directory_sync_failure_after_rename_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "before").unwrap();
        let workspace = workspace(root.path());
        let plan = publish_fault(PublishStep::DirSync);
        let ops = FaultyPublishOps::new(plan.clone());
        let tool = WriteTool::with_publish_ops(&workspace, &ops);

        let result = run(&tool, r#"{"path":"file.txt","content":"after"}"#).await;
        plan.assert_all_fired();

        assert!(result.is_error, "{result:?}");
        let outcome = result
            .outcome_override
            .expect("post-rename failure override");
        assert_eq!(outcome.disposition, OperationDisposition::Error);
        assert_eq!(outcome.effect_certainty, EffectCertainty::Unknown);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "after");
    }

    #[tokio::test]
    async fn rename_failure_keeps_the_file_and_leaves_no_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "before").unwrap();
        let workspace = workspace(root.path());
        let plan = publish_fault(PublishStep::Rename);
        let ops = FaultyPublishOps::new(plan.clone());
        let tool = WriteTool::with_publish_ops(&workspace, &ops);

        let result = run(&tool, r#"{"path":"file.txt","content":"after"}"#).await;

        plan.assert_all_fired();
        assert!(result.is_error, "{result:?}");
        // A plain error result: only a post-rename failure overrides the
        // outcome to `Unknown`.
        assert!(result.outcome_override.is_none(), "{result:?}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before");
        let names: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["file.txt"]);
    }

    #[tokio::test]
    async fn writing_creates_parents_and_leaves_no_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let tool = WriteTool::new(&workspace);

        let nested = run(&tool, r#"{"path":"nested/file.txt","content":"hello"}"#).await;
        assert!(!nested.is_error, "{nested:?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("nested/file.txt")).unwrap(),
            "hello"
        );

        let flat = run(&tool, r#"{"path":"file.txt","content":"hello"}"#).await;
        assert!(!flat.is_error, "{flat:?}");
        let mut names: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["file.txt".to_owned(), "nested".to_owned()]);
    }

    #[tokio::test]
    async fn an_existing_file_keeps_its_permissions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "before").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let workspace = workspace(root.path());

        let result = run(
            &WriteTool::new(&workspace),
            r#"{"path":"file.txt","content":"after"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode = {mode:#o}");
    }

    #[tokio::test]
    async fn a_symlink_escape_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let workspace = workspace(root.path());

        let result = run(
            &WriteTool::new(&workspace),
            r#"{"path":"link/file.txt","content":"hello"}"#,
        )
        .await;
        assert!(
            result.is_error && result.content.contains("escapes workspace"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn the_original_workspace_is_kept_after_its_path_is_replaced() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let workspace = workspace(&root);

        let moved = parent.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();

        let result = run(
            &WriteTool::new(&workspace),
            r#"{"path":"note.txt","content":"inside"}"#,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(moved.join("note.txt")).unwrap(),
            "inside"
        );
        assert!(!outside.path().join("note.txt").exists());
    }

    #[tokio::test]
    async fn a_new_absolute_path_inside_the_workspace_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let arguments = serde_json::json!({
            "path": root.path().join("new.txt").to_string_lossy(),
            "content": "inside",
        })
        .to_string();
        let result = run(&WriteTool::new(&workspace), &arguments).await;
        assert!(!result.is_error, "{result:?}");
    }

    #[tokio::test]
    async fn parent_traversal_through_a_symlink_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let workspace = workspace(root.path());

        let result = run(
            &WriteTool::new(&workspace),
            r#"{"path":"link/../note.txt","content":"nope"}"#,
        )
        .await;
        assert!(result.is_error, "{result:?}");
        assert!(!root.path().join("note.txt").exists());
    }

    #[tokio::test]
    async fn an_internal_final_symlink_is_followed_and_left_in_place() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.txt");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink("target.txt", root.path().join("relative-link")).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("absolute-link")).unwrap();
        let workspace = workspace(root.path());
        let tool = WriteTool::new(&workspace);

        for link in ["relative-link", "absolute-link"] {
            let result = run(&tool, &format!(r#"{{"path":"{link}","content":"write"}}"#)).await;
            assert!(!result.is_error, "write({link}) = {result:?}");
            assert!(
                std::fs::symlink_metadata(root.path().join(link))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "link {link} was replaced"
            );
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "write");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "mode = {mode:#o}");
    }
}
