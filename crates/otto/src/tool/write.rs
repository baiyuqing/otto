//! The `write` tool. Port of `internal/tool/write.go`.
//!
//! Creates or replaces a workspace file. The write is atomic: content goes to a
//! temporary file in the destination directory and is renamed into place
//! through the workspace root handle, so a reader never observes a partial
//! file and the destination is never resolved outside the workspace. An
//! existing file's permission bits are preserved; a new file gets `0644`.

use std::io::Write;
use std::path::Path;

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use super::gopath::{bytes, dir, path_from};
use super::result::decode_strict_json;
use super::root::is_dir;
use super::workspace::Workspace;
use super::{Tool, definition, error_result, text_result};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    #[serde(default)]
    path: String,
    #[serde(default)]
    content: String,
}

/// Writes a workspace file.
pub struct WriteTool<'a> {
    workspace: &'a Workspace,
}

impl<'a> WriteTool<'a> {
    pub fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }
}

/// The schema advertised for `write`.
pub fn write_definition() -> ToolDefinition {
    definition(
        "write",
        "Create or replace a file in the workspace",
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

#[async_trait::async_trait]
impl Tool for WriteTool<'_> {
    fn definition(&self) -> ToolDefinition {
        write_definition()
    }

    async fn execute(&self, arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        let args: WriteArgs = match decode_strict_json(arguments.get(), &["path", "content"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if args.path.is_empty() {
            return error_result("missing required argument: path");
        }
        let relative = match self.workspace.write_relative(Path::new(&args.path)) {
            Ok(relative) => relative,
            Err(error) => return error_result(error),
        };
        let _guard = self.workspace.lock_path(&relative).await;
        if let Err(message) = write_file_atomic(self.workspace, &relative, args.content.as_bytes())
        {
            return error_result(message);
        }
        text_result(format!(
            "wrote {} ({} bytes)",
            args.path,
            args.content.len()
        ))
    }
}

/// Writes `content` to the root-relative `path` through a temporary file and a
/// rename. Port of `writeFileAtomic`.
pub(crate) fn write_file_atomic(
    workspace: &Workspace,
    path: &Path,
    content: &[u8],
) -> Result<(), String> {
    let root = workspace.root_fs();
    let directory = path_from(dir(bytes(path)));
    root.mkdir_all(&directory, Mode::from_bits_truncate(0o755))
        .map_err(|error| error.to_string())?;

    let mut mode = 0o644u32;
    match root.stat(path) {
        Ok(stat) => {
            if is_dir(&stat) {
                return Err(format!("path is a directory: {}", path.display()));
            }
            mode = u32::from(stat.st_mode) & 0o777;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }

    let (temporary_path, mut temporary) = create_workspace_temp(workspace, &directory, mode)?;
    let result = (|| -> std::io::Result<()> {
        temporary.set_permissions(std::fs::Permissions::from_mode(mode))?;
        temporary.write_all(content)?;
        temporary.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        drop(temporary);
        let _ = root.remove(&temporary_path);
        return Err(error.to_string());
    }
    drop(temporary);
    if let Err(error) = root.rename(&temporary_path, path) {
        let _ = root.remove(&temporary_path);
        return Err(error.to_string());
    }
    Ok(())
}

use std::os::unix::fs::PermissionsExt;

/// Creates an exclusive temporary file in `directory`. Port of
/// `createWorkspaceTemp`; the name uses eight bytes from the system random
/// source so a hostile workspace cannot predict and pre-create it.
fn create_workspace_temp(
    workspace: &Workspace,
    directory: &Path,
    mode: u32,
) -> Result<(std::path::PathBuf, std::fs::File), String> {
    for _ in 0..100 {
        let suffix = random_suffix().map_err(|error| error.to_string())?;
        let name = path_from(super::gopath::join(&[
            bytes(directory),
            format!(".otto-{suffix}").as_bytes(),
        ]));
        match workspace.root_fs().open_file(
            &name,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            Mode::from_bits_truncate(mode as nix::sys::stat::mode_t),
        ) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("could not create temporary file".to_owned())
}

fn random_suffix() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::{run, workspace};
    use std::os::unix::fs::PermissionsExt;

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

        let missing = run(&tool, r#"{"content":"hello"}"#).await;
        assert!(
            missing.is_error && missing.content.contains("path"),
            "{missing:?}"
        );
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
