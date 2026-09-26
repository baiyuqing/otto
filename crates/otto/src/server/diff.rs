//! `GET /v1/workspaces/diff`: a read-only view of a working directory's
//! changes against `HEAD`. See `docs/specs/2026-09-26-workspace-diff-review.md`.
//!
//! Nothing here writes to the repository or the working tree: every git
//! command below is a read (`rev-parse`, `diff`, `ls-files`), and
//! `GIT_OPTIONAL_LOCKS=0` keeps `diff`/`ls-files` from refreshing
//! `.git/index` on disk.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{Server, error_response, json_response, workspace_load_error_response};
use crate::sandbox::{CommandExecutor, Request, Streams};

/// The production deadline for every git command combined, per request.
const GIT_DEADLINE: Duration = Duration::from_secs(10);
/// The git object id of the empty tree, used as the diff base before the
/// first commit.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// One file's `patch` is cut to this many bytes, on a line boundary.
const MAX_FILE_PATCH_BYTES: usize = 256 * 1024;
/// The response's total `patch` bytes across every file.
const MAX_TOTAL_PATCH_BYTES: usize = 1024 * 1024;
/// Untracked files past this many (sorted by path) are listed without a
/// patch.
const MAX_UNTRACKED_FILES: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: String,
    pub binary: bool,
    pub patch: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceDiff {
    pub workspace: String,
    pub repository: bool,
    pub branch: Option<String>,
    pub files: Vec<DiffFile>,
    pub truncated: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct DiffQuery {
    workspace: Option<String>,
}

pub async fn get(State(server): State<Arc<Server>>, Query(query): Query<DiffQuery>) -> Response {
    let workspace = match &query.workspace {
        Some(path) => match server.factory.load_workspace(path).await {
            Ok((info, _newly_loaded)) => info.path,
            Err(error) => return workspace_load_error_response(error),
        },
        None => server.factory.workspaces().await.startup,
    };

    let Some((executor, environment)) = server.factory.diff_runner(&workspace).await else {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "diff_unavailable",
            "no usable sandbox for this workspace",
        );
    };

    let runner = GitRunner {
        executor,
        dir: workspace.clone(),
        environment,
        cancel: CancellationToken::new(),
    };

    match build_diff(&runner, &workspace, GIT_DEADLINE).await {
        Ok(diff) => json_response(StatusCode::OK, &diff),
        Err(GitDiffError::Timeout) => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "git_timeout",
            "git commands did not finish in time",
        ),
        Err(GitDiffError::Failed(code)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git_failed",
            &format!("git exited with status {code}"),
        ),
    }
}

// ---- git ----

/// Runs the workspace's read-only git commands through its sandbox
/// [`CommandExecutor`].
struct GitRunner {
    executor: Arc<dyn CommandExecutor>,
    dir: String,
    environment: Vec<String>,
    cancel: CancellationToken,
}

/// Why [`build_diff`] could not finish.
enum GitDiffError {
    /// The combined deadline passed; `cancel` was signaled.
    Timeout,
    /// A command failed: a non-zero exit the caller did not accept, or the
    /// executor itself returned an error.
    Failed(i32),
}

impl GitRunner {
    /// Runs `git <args>` with the fixed sandbox flags
    /// (`-c core.fsmonitor=false -c core.quotepath=off`) and
    /// `GIT_OPTIONAL_LOCKS=0` added to the sandbox environment. Returns the
    /// exit code and stdout for any exit code; the caller decides which
    /// codes are acceptable for that command.
    async fn git(
        &self,
        args: &[&str],
        deadline: tokio::time::Instant,
    ) -> Result<(i32, String), GitDiffError> {
        let mut argv = vec![
            "git".to_string(),
            "-c".to_string(),
            "core.fsmonitor=false".to_string(),
            "-c".to_string(),
            "core.quotepath=off".to_string(),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        let mut environment = self.environment.clone();
        environment.push("GIT_OPTIONAL_LOCKS=0".to_string());
        let request = Request {
            argv,
            dir: PathBuf::from(&self.dir),
            env: environment,
        };
        let mut out: Vec<u8> = Vec::new();
        let mut discard = std::io::sink();
        let streams = Streams {
            stdout: &mut out,
            stderr: &mut discard,
        };
        let execution = self.executor.execute(request, streams, &self.cancel);
        match tokio::time::timeout_at(deadline, execution).await {
            Err(_) => {
                self.cancel.cancel();
                Err(GitDiffError::Timeout)
            }
            Ok((status, Err(_))) => Err(GitDiffError::Failed(status.code)),
            Ok((status, Ok(()))) => Ok((status.code, String::from_utf8_lossy(&out).into_owned())),
        }
    }
}

// ---- splitting ----

/// Splits `git diff`/`git diff --no-index` output at each `diff --git ` line
/// and reads each file's status from its header lines. `force_status`
/// overrides the parsed status (used for untracked files, whose `--no-index`
/// output otherwise looks like an added file).
fn split_diff_output(output: &str, force_status: Option<&str>) -> Vec<DiffFile> {
    let mut starts = Vec::new();
    let mut offset = 0usize;
    for line in output.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(output.len());
            parse_file_segment(&output[start..end], force_status)
        })
        .collect()
}

/// One file's header fields, gathered before path/status are resolved.
#[derive(Default)]
struct DiffHeader {
    new_file: bool,
    deleted_file: bool,
    rename_from: Option<String>,
    rename_to: Option<String>,
    binary: bool,
    plus_path: Option<String>,
    minus_path: Option<String>,
    binary_old: Option<String>,
    binary_new: Option<String>,
}

impl DiffHeader {
    fn absorb(&mut self, line: &str) {
        if line.starts_with("new file mode") {
            self.new_file = true;
        } else if line.starts_with("deleted file mode") {
            self.deleted_file = true;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            self.rename_from = Some(path.to_string());
        } else if let Some(path) = line.strip_prefix("rename to ") {
            self.rename_to = Some(path.to_string());
        } else if let Some(path) = line.strip_prefix("+++ ") {
            self.plus_path = side_path(path, "b/");
        } else if let Some(path) = line.strip_prefix("--- ") {
            self.minus_path = side_path(path, "a/");
        } else if let Some(rest) = line
            .strip_prefix("Binary files ")
            .and_then(|rest| rest.strip_suffix(" differ"))
        {
            self.binary = true;
            if let Some((old, new)) = rest.split_once(" and ") {
                self.binary_old = side_path(old, "a/");
                self.binary_new = side_path(new, "b/");
            }
        }
    }

    /// `modified` unless the header says otherwise; `force_status` wins over
    /// all of it (untracked files parse like an added file otherwise).
    fn status(&self, force_status: Option<&str>) -> String {
        if let Some(status) = force_status {
            return status.to_string();
        }
        if self.rename_from.is_some() && self.rename_to.is_some() {
            "renamed".to_string()
        } else if self.new_file {
            "added".to_string()
        } else if self.deleted_file {
            "deleted".to_string()
        } else {
            "modified".to_string()
        }
    }

    /// The file's current path. Renames carry it unambiguously in `rename
    /// to`; every other case falls back to the `+++`/`---`/`Binary files`
    /// line, whichever the diff actually has for that status. The
    /// `diff --git` line is a last resort for the rare diff with none of
    /// those (a pure file-mode change), so a file is never silently dropped.
    fn path(&self, diff_git_line: &str) -> Option<String> {
        self.rename_to
            .clone()
            .or_else(|| self.plus_path.clone())
            .or_else(|| self.binary_new.clone())
            .or_else(|| self.minus_path.clone())
            .or_else(|| self.binary_old.clone())
            .or_else(|| fallback_path_from_diff_git_line(diff_git_line))
    }
}

/// Strips the `a/`/`b/` prefix from one side of a `+++`/`---`/`Binary files`
/// line; `/dev/null` (no real file on that side) is `None`.
fn side_path(text: &str, prefix: &str) -> Option<String> {
    // git ends the line with a tab when the path contains a space.
    let text = text.strip_suffix('\t').unwrap_or(text);
    if text == "/dev/null" {
        None
    } else {
        Some(text.strip_prefix(prefix).unwrap_or(text).to_string())
    }
}

/// ponytail: only used for a diff with none of the usual header lines (a
/// pure file-mode change); real content/rename/binary diffs never reach
/// this. Ambiguous for a path containing " b/", which `-M`'s renames avoid
/// by using unambiguous `rename to` lines instead.
fn fallback_path_from_diff_git_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git a/")?;
    let at = rest.rfind(" b/")?;
    Some(
        rest[at + " b/".len()..]
            .trim_end_matches(['\n', '\r'])
            .to_string(),
    )
}

fn parse_file_segment(segment: &str, force_status: Option<&str>) -> Option<DiffFile> {
    let mut header = DiffHeader::default();
    let mut cursor = 0usize;
    let mut patch = "";
    let mut diff_git_line = "";
    for (index, line) in segment.split_inclusive('\n').enumerate() {
        if index == 0 {
            diff_git_line = line;
            cursor += line.len();
            continue;
        }
        let trimmed = line.trim_end_matches('\n');
        if trimmed.starts_with("@@") {
            patch = &segment[cursor..];
            break;
        }
        header.absorb(trimmed);
        cursor += line.len();
    }
    let path = header.path(diff_git_line)?;
    let status = header.status(force_status);
    let old_path = if status == "renamed" {
        header.rename_from.clone()
    } else {
        None
    };
    Some(DiffFile {
        path,
        old_path,
        status,
        binary: header.binary,
        patch: patch.to_string(),
        truncated: false,
    })
}

// ---- limits ----

/// Cuts `patch` to [`MAX_FILE_PATCH_BYTES`], on a line boundary. Returns the
/// possibly-unchanged patch and whether it was cut.
fn cut_patch_to_file_limit(patch: String) -> (String, bool) {
    if patch.len() <= MAX_FILE_PATCH_BYTES {
        return (patch, false);
    }
    let mut cut = MAX_FILE_PATCH_BYTES;
    while cut > 0 && patch.as_bytes()[cut - 1] != b'\n' {
        cut -= 1;
    }
    (patch[..cut].to_string(), true)
}

/// Applies the [`MAX_TOTAL_PATCH_BYTES`] response-wide limit to `files`,
/// already in their final order: once the running total exceeds the limit,
/// later files have their patch cleared and marked truncated. Returns
/// whether any file was cut this way.
fn apply_total_patch_limit(files: &mut [DiffFile]) -> bool {
    let mut total = 0usize;
    let mut truncated = false;
    for file in files.iter_mut() {
        if total >= MAX_TOTAL_PATCH_BYTES {
            if !file.patch.is_empty() {
                file.patch.clear();
                file.truncated = true;
            }
            truncated = true;
            continue;
        }
        total += file.patch.len();
    }
    truncated
}

// ---- assembly ----

/// Builds the full [`WorkspaceDiff`] by running the git commands in order.
async fn build_diff(
    runner: &GitRunner,
    workspace: &str,
    deadline_from_now: Duration,
) -> Result<WorkspaceDiff, GitDiffError> {
    let deadline = tokio::time::Instant::now() + deadline_from_now;

    let (code, _) = runner
        .git(&["rev-parse", "--is-inside-work-tree"], deadline)
        .await?;
    if code != 0 {
        return Ok(WorkspaceDiff {
            workspace: workspace.to_string(),
            repository: false,
            branch: None,
            files: Vec::new(),
            truncated: false,
        });
    }

    let (code, branch_out) = runner
        .git(&["rev-parse", "--abbrev-ref", "HEAD"], deadline)
        .await?;
    let (branch, base) = if code == 0 {
        (Some(branch_out.trim().to_string()), "HEAD".to_string())
    } else {
        (None, EMPTY_TREE.to_string())
    };

    let (code, diff_out) = runner
        .git(
            &[
                "diff",
                &base,
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                // Paths relative to the workspace, as `ls-files` prints them,
                // when the workspace is a subdirectory of the repository.
                "--relative",
                "-M",
                "--",
                ".",
            ],
            deadline,
        )
        .await?;
    if code != 0 {
        return Err(GitDiffError::Failed(code));
    }
    let mut files = split_diff_output(&diff_out, None);

    let (code, ls_out) = runner
        .git(
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
            ],
            deadline,
        )
        .await?;
    if code != 0 {
        return Err(GitDiffError::Failed(code));
    }
    let mut untracked_paths: Vec<&str> =
        ls_out.split('\0').filter(|path| !path.is_empty()).collect();
    untracked_paths.sort_unstable();

    let mut untracked_overflow = false;
    for (index, path) in untracked_paths.iter().enumerate() {
        if index >= MAX_UNTRACKED_FILES {
            untracked_overflow = true;
            files.push(DiffFile {
                path: (*path).to_string(),
                old_path: None,
                status: "untracked".to_string(),
                binary: false,
                patch: String::new(),
                truncated: true,
            });
            continue;
        }
        let (code, out) = runner
            .git(
                &[
                    "diff",
                    "--no-index",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--",
                    "/dev/null",
                    path,
                ],
                deadline,
            )
            .await?;
        if code != 0 && code != 1 {
            return Err(GitDiffError::Failed(code));
        }
        files.extend(split_diff_output(&out, Some("untracked")));
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));

    let mut truncated = untracked_overflow;
    for file in files.iter_mut() {
        let (patch, cut) = cut_patch_to_file_limit(std::mem::take(&mut file.patch));
        file.patch = patch;
        if cut {
            file.truncated = true;
            truncated = true;
        }
    }
    if apply_total_patch_limit(&mut files) {
        truncated = true;
    }

    Ok(WorkspaceDiff {
        workspace: workspace.to_string(),
        repository: true,
        branch,
        files,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 1: splitter ----

    #[test]
    fn splits_a_modified_file() {
        let output = "diff --git a/src/a.rs b/src/a.rs\nindex 111..222 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,4 @@\n line\n+new line\n line\n";
        let files = split_diff_output(output, None);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].old_path, None);
        assert_eq!(files[0].status, "modified");
        assert!(!files[0].binary);
        assert_eq!(files[0].patch, "@@ -1,3 +1,4 @@\n line\n+new line\n line\n");
    }

    #[test]
    fn a_path_with_a_space_drops_the_tab_git_appends() {
        // git ends a `---`/`+++` line with a tab when the path has a space.
        let output = "diff --git a/my file.txt b/my file.txt\nindex 111..222 100644\n--- a/my file.txt\t\n+++ b/my file.txt\t\n@@ -1 +1 @@\n-a\n+b\n";
        let files = split_diff_output(output, None);
        assert_eq!(files[0].path, "my file.txt");
    }

    #[test]
    fn splits_an_added_file() {
        let output = "diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 000..111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n";
        let files = split_diff_output(output, None);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].old_path, None);
        assert_eq!(files[0].status, "added");
    }

    #[test]
    fn splits_a_deleted_file() {
        let output = "diff --git a/old.txt b/old.txt\ndeleted file mode 100644\nindex 111..000\n--- a/old.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n";
        let files = split_diff_output(output, None);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "old.txt");
        assert_eq!(files[0].status, "deleted");
    }

    #[test]
    fn splits_a_renamed_file() {
        let output = "diff --git a/old.txt b/new.txt\nsimilarity index 66%\nrename from old.txt\nrename to new.txt\nindex 111..222 100644\n--- a/old.txt\n+++ b/new.txt\n@@ -1,3 +1,4 @@\n line1\n+appended\n";
        let files = split_diff_output(output, None);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("old.txt"));
        assert_eq!(files[0].status, "renamed");
    }

    #[test]
    fn a_pure_rename_with_no_content_change_has_an_empty_patch() {
        let output = "diff --git a/old.txt b/new.txt\nsimilarity index 100%\nrename from old.txt\nrename to new.txt\n";
        let files = split_diff_output(output, None);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, "renamed");
        assert_eq!(files[0].patch, "");
    }

    #[test]
    fn splits_a_binary_file() {
        let output = "diff --git a/img.bin b/img.bin\nindex 111..222 100644\nBinary files a/img.bin and b/img.bin differ\n";
        let files = split_diff_output(output, None);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "img.bin");
        assert!(files[0].binary);
        assert_eq!(files[0].patch, "");
    }

    #[test]
    fn splits_several_files_in_one_diff_output() {
        let output = "diff --git a/a.txt b/a.txt\ndeleted file mode 100644\nindex 111..000\n--- a/a.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-a\ndiff --git a/img.bin b/img.bin\nindex 111..222 100644\nBinary files a/img.bin and b/img.bin differ\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 000..111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,1 @@\n+new content\n";
        let files = split_diff_output(output, None);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].status, "deleted");
        assert_eq!(files[1].path, "img.bin");
        assert!(files[1].binary);
        assert_eq!(files[2].path, "new.txt");
        assert_eq!(files[2].status, "added");
    }

    #[test]
    fn force_status_overrides_the_parsed_status_for_untracked_files() {
        let output = "diff --git a/untracked.txt b/untracked.txt\nnew file mode 100644\nindex 000..111\n--- /dev/null\n+++ b/untracked.txt\n@@ -0,0 +1,1 @@\n+hello\n";
        let files = split_diff_output(output, Some("untracked"));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, "untracked");
        assert_eq!(files[0].path, "untracked.txt");
    }

    // ---- 2: limits ----

    #[test]
    fn a_patch_over_the_per_file_limit_is_cut_on_a_line_boundary() {
        let line = "+".to_string() + &"x".repeat(100) + "\n";
        let mut patch = String::new();
        while patch.len() <= MAX_FILE_PATCH_BYTES {
            patch.push_str(&line);
        }
        let original_len = patch.len();
        let (cut, truncated) = cut_patch_to_file_limit(patch);
        assert!(truncated);
        assert!(cut.len() <= MAX_FILE_PATCH_BYTES);
        assert!(cut.len() < original_len);
        assert!(cut.ends_with('\n'), "cut on a line boundary: {cut:?}");
    }

    #[test]
    fn a_patch_under_the_per_file_limit_is_unchanged() {
        let patch = "@@ -1 +1 @@\n-a\n+b\n".to_string();
        let (cut, truncated) = cut_patch_to_file_limit(patch.clone());
        assert!(!truncated);
        assert_eq!(cut, patch);
    }

    #[test]
    fn files_after_the_total_limit_are_cleared_and_marked_truncated() {
        let big = "x".repeat(MAX_TOTAL_PATCH_BYTES);
        let mut files = vec![
            DiffFile {
                path: "a.txt".to_string(),
                old_path: None,
                status: "modified".to_string(),
                binary: false,
                patch: big,
                truncated: false,
            },
            DiffFile {
                path: "b.txt".to_string(),
                old_path: None,
                status: "modified".to_string(),
                binary: false,
                patch: "small".to_string(),
                truncated: false,
            },
        ];
        let cut = apply_total_patch_limit(&mut files);
        assert!(cut);
        assert!(!files[0].patch.is_empty());
        assert!(files[1].patch.is_empty());
        assert!(files[1].truncated);
    }

    #[test]
    fn files_under_the_total_limit_are_unaffected() {
        let mut files = vec![DiffFile {
            path: "a.txt".to_string(),
            old_path: None,
            status: "modified".to_string(),
            binary: false,
            patch: "small".to_string(),
            truncated: false,
        }];
        let cut = apply_total_patch_limit(&mut files);
        assert!(!cut);
        assert_eq!(files[0].patch, "small");
    }
}
