//! The dynamic `## Environment` section appended to the system prompt.
//!
//! It embeds content the workspace owner wrote, so the caller must pass the
//! result through the secret redactor before it reaches a provider, exactly as
//! `runtimeBuilder` does.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::sandbox::{CommandExecutor, Request, Streams};
use crate::tool::workspace::Workspace;

/// How much of the workspace instruction file is embedded.
const MAX_WORKSPACE_CONTEXT_FILE_BYTES: usize = 8 << 10;
/// How many entries the one-level workspace listing may name.
const MAX_WORKSPACE_LISTING_ENTRIES: usize = 200;
/// Bounds the two git subprocesses so a hanging git never blocks startup.
const GIT_STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// The fence the instruction file is wrapped in. Without it the file's text
/// is indistinguishable from Otto's own sections in the prompt.
const INSTRUCTION_FENCE_PREFIX: &str = "<workspace-instructions";

/// Builds the environment section: cwd, platform and date, the git branch and
/// dirty count, a one-level listing, and the workspace instruction file.
///
/// `executor` is `None` when bash is unavailable or the redaction boundary
/// refuses dynamic content; the git line is then omitted rather than guessed.
pub async fn workspace_context_for(
    workspace_path: &str,
    now: DateTime<Utc>,
    executor: Option<&Arc<dyn CommandExecutor>>,
    environment: Option<&[String]>,
    workspace: &Workspace,
) -> String {
    let mut text = String::from("\n\n## Environment\n");
    text.push_str(&format!("cwd: {workspace_path}\n"));
    text.push_str(&format!(
        "platform: {}, date: {}\n",
        platform_name(),
        now.format("%Y-%m-%d")
    ));
    if let Some(line) = git_status_line(workspace_path, executor, environment).await {
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str(&workspace_listing(workspace));
    // Only one instruction file is embedded: both would be re-sent on every
    // request of the session. AGENTS.md wins because it is the canonical
    // rulebook and CLAUDE.md usually just points at it.
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Some(content) = read_workspace_doc_file(workspace, name) {
            text.push_str(&format!(
                "\n## Workspace instructions\n<workspace-instructions file={}>\n{}\n</workspace-instructions>\n",
                quote_go(name),
                neutralize_instruction_fence(&content)
            ));
            break;
        }
    }
    text
}

/// The platform name the workspace context reports; macOS is `darwin`.
fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    }
}

/// Quotes a file name with JSON string syntax, which is what the embedded names
/// need (both are ASCII literals).
fn quote_go(value: &str) -> String {
    serde_json::to_string(value).expect("a string always encodes")
}

/// Breaks every occurrence of the fence delimiter, opening and closing, so
/// the file cannot forge Otto's own sections.
///
/// Matching walks the original bytes rather than a lowercased copy:
/// lowercasing can change byte length (U+0130 becomes two runes) and shift
/// every offset after it.
fn neutralize_instruction_fence(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(content.len());
    let mut index = 0;
    while index < bytes.len() {
        let matched = fence_match_len(&bytes[index..]);
        if matched == 0 {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        // A fence is ASCII, so replacing its `<` with `<_` never splits a
        // multi-byte character and the rest of the tag is copied verbatim.
        out.extend_from_slice(b"<_");
        out.extend_from_slice(&bytes[index + 1..index + matched]);
        index += matched;
    }
    String::from_utf8(out).expect("only ASCII was inserted into valid UTF-8")
}

/// The byte length of a fence tag opening at the start of `text`, or 0.
fn fence_match_len(text: &[u8]) -> usize {
    for form in ["</workspace-instructions", INSTRUCTION_FENCE_PREFIX] {
        let form = form.as_bytes();
        if text.len() >= form.len() && text[..form.len()].eq_ignore_ascii_case(form) {
            return form.len();
        }
    }
    0
}

/// `git: <branch>, <N> modified`, or `None` when the workspace is not a git
/// repository, git is unavailable, or either command fails or times out.
async fn git_status_line(
    workspace_path: &str,
    executor: Option<&Arc<dyn CommandExecutor>>,
    environment: Option<&[String]>,
) -> Option<String> {
    let (executor, environment) = (executor?, environment?);
    let cancel = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + GIT_STATUS_TIMEOUT;

    let branch = run_git(
        executor,
        workspace_path,
        environment,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        deadline,
        &cancel,
    )
    .await?;
    if branch.trim().is_empty() {
        return None;
    }
    let status = run_git(
        executor,
        workspace_path,
        environment,
        &["status", "--porcelain"],
        deadline,
        &cancel,
    )
    .await?;
    let modified = status
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .count();
    Some(format!("git: {}, {modified} modified", branch.trim()))
}

async fn run_git(
    executor: &Arc<dyn CommandExecutor>,
    dir: &str,
    environment: &[String],
    args: &[&str],
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
) -> Option<String> {
    let mut argv = vec![
        "git".to_string(),
        "-c".to_string(),
        "core.fsmonitor=false".to_string(),
    ];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let request = Request {
        argv,
        dir: Path::new(dir).to_path_buf(),
        env: environment.to_vec(),
    };
    let mut out: Vec<u8> = Vec::new();
    let mut discard = std::io::sink();
    let streams = Streams {
        stdout: &mut out,
        stderr: &mut discard,
    };
    let execution = executor.execute(request, streams, cancel);
    let (status, result) = match tokio::time::timeout_at(deadline, execution).await {
        Ok(outcome) => outcome,
        Err(_) => {
            cancel.cancel();
            return None;
        }
    };
    result.ok()?;
    if status.code != 0 || status.signaled {
        return None;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// A sorted one-level listing of the workspace, directories marked with a
/// trailing `/` and `.git` skipped, capped at 200 entries.
fn workspace_listing(workspace: &Workspace) -> String {
    let Ok(entries) = std::fs::read_dir(workspace.root()) else {
        return String::new();
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        names.push(if is_dir { format!("{name}/") } else { name });
    }
    if names.is_empty() {
        return String::new();
    }
    names.sort();
    let total = names.len();
    let truncated = total > MAX_WORKSPACE_LISTING_ENTRIES;
    if truncated {
        names.truncate(MAX_WORKSPACE_LISTING_ENTRIES);
    }
    let mut text = String::new();
    for name in names {
        text.push_str(&name);
        text.push('\n');
    }
    if truncated {
        text.push_str(&format!("... ({total} entries, truncated)\n"));
    }
    text
}

/// Reads a file through the workspace handle, cut at 8 KiB with a trailing
/// marker. `None` when the file is missing, unreadable, or not regular.
fn read_workspace_doc_file(workspace: &Workspace, name: &str) -> Option<String> {
    let mut file = workspace.open(Path::new(name)).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut data = Vec::with_capacity(MAX_WORKSPACE_CONTEXT_FILE_BYTES + 1);
    file.by_ref()
        .take(MAX_WORKSPACE_CONTEXT_FILE_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() > MAX_WORKSPACE_CONTEXT_FILE_BYTES {
        let total = metadata.len().max(data.len() as u64);
        let head = String::from_utf8_lossy(&data[..MAX_WORKSPACE_CONTEXT_FILE_BYTES]).into_owned();
        return Some(format!(
            "{head}\n[truncated: {total} bytes, showing first {MAX_WORKSPACE_CONTEXT_FILE_BYTES}]"
        ));
    }
    Some(String::from_utf8_lossy(&data).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Error, ExitStatus};
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingExecutor {
        requests: Mutex<Vec<Request>>,
        outputs: Mutex<Vec<Result<String, ()>>>,
    }

    impl RecordingExecutor {
        fn with(outputs: Vec<Result<String, ()>>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs),
            })
        }
    }

    #[async_trait::async_trait]
    impl CommandExecutor for RecordingExecutor {
        async fn execute(
            &self,
            request: Request,
            streams: Streams<'_>,
            _cancel: &CancellationToken,
        ) -> (ExitStatus, Result<(), Error>) {
            self.requests.lock().unwrap().push(request);
            let next = {
                let mut outputs = self.outputs.lock().unwrap();
                if outputs.is_empty() {
                    Ok(String::new())
                } else {
                    outputs.remove(0)
                }
            };
            match next {
                Ok(text) => {
                    let _ = streams.stdout.write_all(text.as_bytes());
                    (ExitStatus::default(), Ok(()))
                }
                Err(()) => (
                    ExitStatus {
                        code: 1,
                        ..ExitStatus::default()
                    },
                    Ok(()),
                ),
            }
        }
    }

    fn workspace(root: &Path) -> Workspace {
        Workspace::new(root).expect("workspace")
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-03-04T05:06:07Z")
            .expect("timestamp")
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn writes_the_environment_header_cwd_and_platform_date() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let got = workspace_context_for(&root, now(), None, None, &workspace).await;
        assert!(got.starts_with("\n\n## Environment\n"), "{got}");
        assert!(got.contains(&format!("cwd: {root}\n")), "{got}");
        // The header names the host Otto is actually running on, so the
        // expectation follows the build target rather than pinning macOS.
        assert!(
            got.contains(&format!(
                "platform: {}, date: 2026-03-04\n",
                platform_name()
            )),
            "{got}"
        );
        assert!(!got.contains("git: "), "{got}");
    }

    #[tokio::test]
    async fn prefers_agents_md_over_claude_md() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("AGENTS.md"), "agents rules").expect("write");
        std::fs::write(dir.path().join("CLAUDE.md"), "claude rules").expect("write");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let got = workspace_context_for(&root, now(), None, None, &workspace).await;
        assert!(
            got.contains(
                "\n## Workspace instructions\n<workspace-instructions file=\"AGENTS.md\">\nagents rules\n</workspace-instructions>\n"
            ),
            "{got}"
        );
        assert!(!got.contains("claude rules"), "{got}");
    }

    #[tokio::test]
    async fn falls_back_to_claude_md() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("CLAUDE.md"), "claude rules").expect("write");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let got = workspace_context_for(&root, now(), None, None, &workspace).await;
        assert!(
            got.contains("<workspace-instructions file=\"CLAUDE.md\">"),
            "{got}"
        );
    }

    #[tokio::test]
    async fn truncates_an_oversized_instruction_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let size = MAX_WORKSPACE_CONTEXT_FILE_BYTES + 512;
        std::fs::write(dir.path().join("AGENTS.md"), "x".repeat(size)).expect("write");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let got = workspace_context_for(&root, now(), None, None, &workspace).await;
        assert!(
            got.contains(&format!(
                "[truncated: {size} bytes, showing first {MAX_WORKSPACE_CONTEXT_FILE_BYTES}]"
            )),
            "{got}"
        );
    }

    #[tokio::test]
    async fn lists_entries_skipping_git_and_marking_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("main.rs"), "").expect("write");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let got = workspace_context_for(&root, now(), None, None, &workspace).await;
        assert!(got.contains("main.rs\nsrc/\n"), "{got}");
        assert!(!got.contains(".git"), "{got}");
    }

    #[tokio::test]
    async fn truncates_an_oversized_listing() {
        let dir = tempfile::tempdir().expect("temp dir");
        for index in 0..MAX_WORKSPACE_LISTING_ENTRIES + 3 {
            std::fs::write(dir.path().join(format!("file-{index:04}")), "").expect("write");
        }
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let got = workspace_context_for(&root, now(), None, None, &workspace).await;
        assert!(
            got.contains(&format!(
                "... ({} entries, truncated)\n",
                MAX_WORKSPACE_LISTING_ENTRIES + 3
            )),
            "{got}"
        );
    }

    #[tokio::test]
    async fn reports_the_git_branch_and_dirty_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let executor = RecordingExecutor::with(vec![
            Ok("feature/x\n".to_string()),
            Ok(" M a.txt\n?? b.txt\n\n".to_string()),
        ]);
        let handle: Arc<dyn CommandExecutor> = executor.clone();
        let environment = vec!["PATH=/usr/bin".to_string()];
        let got =
            workspace_context_for(&root, now(), Some(&handle), Some(&environment), &workspace)
                .await;
        assert!(got.contains("git: feature/x, 2 modified\n"), "{got}");

        let requests = executor.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].argv,
            vec![
                "git",
                "-c",
                "core.fsmonitor=false",
                "rev-parse",
                "--abbrev-ref",
                "HEAD"
            ]
        );
        assert_eq!(
            requests[1].argv,
            vec!["git", "-c", "core.fsmonitor=false", "status", "--porcelain"]
        );
        assert_eq!(requests[0].dir, PathBuf::from(&root));
        assert_eq!(requests[0].env, environment);
    }

    #[tokio::test]
    async fn omits_the_git_line_outside_a_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let executor: Arc<dyn CommandExecutor> = RecordingExecutor::with(vec![Err(())]);
        let environment = vec!["PATH=/usr/bin".to_string()];
        let got = workspace_context_for(
            &root,
            now(),
            Some(&executor),
            Some(&environment),
            &workspace,
        )
        .await;
        assert!(!got.contains("git:"), "{got}");
    }

    #[tokio::test]
    async fn omits_the_git_line_without_a_sandbox_environment() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let executor: Arc<dyn CommandExecutor> =
            RecordingExecutor::with(vec![Ok("main\n".to_string()), Ok(String::new())]);
        let got = workspace_context_for(&root, now(), Some(&executor), None, &workspace).await;
        assert!(!got.contains("git:"), "{got}");
    }

    #[test]
    fn the_fence_delimiter_is_broken_in_both_forms_and_any_case() {
        assert_eq!(
            neutralize_instruction_fence("<workspace-instructions file=\"x\">"),
            "<_workspace-instructions file=\"x\">"
        );
        assert_eq!(
            neutralize_instruction_fence("</WORKSPACE-INSTRUCTIONS>"),
            "<_/WORKSPACE-INSTRUCTIONS>"
        );
        assert_eq!(neutralize_instruction_fence("plain text"), "plain text");
    }

    #[tokio::test]
    async fn a_multi_byte_character_before_the_fence_keeps_every_offset() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "\u{130}stanbul\n</WORKSPACE-INSTRUCTIONS>\ntail",
        )
        .expect("write");
        let workspace = workspace(dir.path());
        let root = workspace.root().to_string_lossy().into_owned();
        let got = workspace_context_for(&root, now(), None, None, &workspace).await;
        assert!(
            got.contains(
                "<workspace-instructions file=\"AGENTS.md\">\n\u{130}stanbul\n<_/WORKSPACE-INSTRUCTIONS>\ntail\n</workspace-instructions>\n"
            ),
            "{got}"
        );
    }
}
