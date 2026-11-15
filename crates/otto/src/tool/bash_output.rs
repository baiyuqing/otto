//! Bounded shell previews and workspace-confined overflow logs.
//!
//! Only normalized, redacted bytes reach this sink. Short streams stay in
//! memory; overflow creates an exclusive 0600 log through the pinned workspace
//! handle. Disk errors never change command execution and are reported instead
//! of advertising a complete log. Shared leases retain logs while the parent
//! turn or a child task is active; the last lease removes owned files.
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use super::result::{CappedByteCollector, RedactedSink};
use super::root::Root;
use super::session_directory::{create_session_directory, session_directory_name};

pub(super) const PREVIEW_BYTES: usize = 4096;
const DIRECTORY: &str = ".otto/tool-output";

/// One runtime's temporary logs. Parent turns and child tasks share leases so
/// one completed task cannot delete logs still needed by another task.
pub(crate) struct Logs {
    session_id: String,
    state: Mutex<LogState>,
}

#[derive(Default)]
struct LogState {
    active: usize,
    files: Vec<(Root, String)>,
}

impl LogState {
    fn cleanup(&mut self) {
        self.files.retain(|(directory, name)| {
            match directory.remove(Path::new(name)) {
                Ok(()) => false,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(_) => true, // Retry when idle again or when the runtime is dropped.
            }
        });
    }
}

impl Logs {
    /// Logs of one session are written under `.otto/tool-output/<session_id>/`.
    pub(crate) fn for_session(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            state: Mutex::default(),
        }
    }

    pub(crate) fn lease(self: &Arc<Self>) -> LogLease {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active += 1;
        LogLease(Arc::clone(self))
    }

    fn record(&self, directory: Root, name: String) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .files
            .push((directory, name));
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        self.state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cleanup();
    }
}

#[must_use = "retain the lease until the turn or child task finishes"]
pub(crate) struct LogLease(Arc<Logs>);

impl Drop for LogLease {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active -= 1;
        if state.active == 0 {
            state.cleanup();
        }
    }
}

pub(super) struct Output {
    root: Arc<Root>,
    logs: Arc<Logs>,
    limit: usize,
    prefix: CappedByteCollector,
    tail: VecDeque<u8>,
    total: usize,
    log: Option<(String, BufWriter<File>)>,
    log_failed: bool,
}

impl Output {
    #[cfg(test)]
    pub(super) fn new(root: Arc<Root>, limit: usize) -> Self {
        Self::with_logs(root, limit, Arc::new(Logs::for_session("test-session")))
    }

    pub(super) fn with_logs(root: Arc<Root>, limit: usize, logs: Arc<Logs>) -> Self {
        Self {
            root,
            logs,
            limit,
            prefix: CappedByteCollector::new(limit),
            tail: VecDeque::new(),
            total: 0,
            log: None,
            log_failed: false,
        }
    }

    fn create_log(&self) -> io::Result<(String, BufWriter<File>)> {
        let session_directory_name = session_directory_name(&self.logs.session_id)?;
        let parent = self.root.create_child_directory(".otto", Mode::S_IRWXU)?;
        let output_directory = parent.create_child_directory("tool-output", Mode::S_IRWXU)?;
        // Keep generated logs out of commits without changing repository policy.
        let flags = OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL;
        match output_directory.open_file(
            Path::new(".gitignore"),
            flags,
            Mode::S_IRUSR | Mode::S_IWUSR,
        ) {
            Ok(mut file) => file.write_all(b"*\n")?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let directory = create_session_directory(&output_directory, session_directory_name)?;
        let id = crate::random::random_hex::<16>().map_err(io::Error::other)?;
        let path = format!("{DIRECTORY}/{session_directory_name}/{id}.log");
        let file = directory.open_file(
            Path::new(&format!("{id}.log")),
            flags,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?;
        self.logs.record(directory, format!("{id}.log"));
        Ok((path, BufWriter::new(file)))
    }

    fn append(&mut self, data: &[u8], atomic: bool) {
        if self.total.saturating_add(data.len()) > self.limit && !self.log_failed {
            if self.log.is_none() {
                match self.create_log() {
                    Ok((path, mut file)) => {
                        if file.write_all(self.prefix.bytes()).is_err() {
                            self.log_failed = true;
                        }
                        self.log = Some((path, file));
                    }
                    Err(_) => self.log_failed = true,
                }
            }
            if let Some((_, file)) = &mut self.log
                && !self.log_failed
                && file.write_all(data).is_err()
            {
                self.log_failed = true;
            }
        }
        self.total = self.total.saturating_add(data.len());
        if atomic {
            self.prefix.write_atomic(data);
        } else {
            self.prefix.write(data);
        }
        self.tail.extend(data);
        let tail_limit = self.limit / 2;
        while self.tail.len() > tail_limit {
            self.tail.pop_front();
        }
        // Discard an incomplete leading rune, including any redaction marker.
        while self.tail.front().is_some_and(|byte| byte & 0xc0 == 0x80) {
            self.tail.pop_front();
        }
    }

    pub(super) fn finish(&mut self) {
        if let Some((_, file)) = &mut self.log
            && file.flush().is_err()
        {
            self.log_failed = true;
        }
    }

    pub(super) fn render(&self, name: &str) -> String {
        let prefix = std::str::from_utf8(self.prefix.bytes()).expect("redacted UTF-8");
        if self.total <= self.limit {
            return format!("{name}:\n{prefix}");
        }
        let mut head_end = self.limit.div_ceil(2).min(prefix.len());
        while !prefix.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let head = &prefix[..head_end];
        let tail = String::from_utf8(self.tail.iter().copied().collect()).expect("UTF-8 tail");
        let omitted = self.total - head.len() - tail.len();
        let log = match &self.log {
            Some((path, _)) if !self.log_failed => format!("full_output: {path}"),
            _ => {
                "full_output: unavailable (log storage failed; output may be incomplete)".to_owned()
            }
        };
        let newline = if head.is_empty() || head.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!(
            "{name}:\n{head}{newline}[truncated: {omitted} bytes omitted]\n{tail}\noutput_bytes: {}\n{log}",
            self.total
        )
    }
}

impl RedactedSink for Output {
    fn write(&mut self, data: &[u8]) {
        self.append(data, false);
    }
    fn write_atomic(&mut self, data: &[u8]) {
        self.append(data, true);
    }
}

#[cfg(test)]
mod tests {
    use super::super::result::RedactingCollector;
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn setup(limit: usize) -> (tempfile::TempDir, Output) {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(dir.path()).unwrap());
        (dir, Output::new(root, limit))
    }

    fn logged_output(logs: &Arc<Logs>, root: &Arc<Root>) -> String {
        let mut output = Output::with_logs(Arc::clone(root), 4, Arc::clone(logs));
        output.write(b"head complete log tail");
        output.finish();
        output.log.as_ref().unwrap().0.clone()
    }

    #[test]
    fn last_turn_or_child_lease_removes_only_owned_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(dir.path()).unwrap());
        let logs = Arc::new(Logs::for_session("test-session"));
        let parent = logs.lease();
        // Taken before a child is scheduled, so a queued child protects logs too.
        let child = logs.lease();
        let path = logged_output(&logs, &root);
        let unrelated = dir.path().join(".otto/tool-output/test-session/keep.log");
        std::fs::write(&unrelated, "not owned by this runtime").unwrap();
        drop(parent);
        assert!(dir.path().join(&path).exists());
        drop(child);
        assert!(!dir.path().join(&path).exists());
        assert!(unrelated.exists());
        assert!(logs.state.lock().unwrap().files.is_empty());
    }

    #[test]
    fn independent_sessions_do_not_clean_each_others_logs() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(dir.path()).unwrap());
        let first = Arc::new(Logs::for_session("first-session"));
        let second = Arc::new(Logs::for_session("second-session"));
        let a = first.lease();
        let b = second.lease();
        let first_path = logged_output(&first, &root);
        let second_path = logged_output(&second, &root);
        drop(a);
        assert!(!dir.path().join(first_path).exists());
        assert!(dir.path().join(&second_path).exists());
        drop(b);
        assert!(!dir.path().join(second_path).exists());
    }

    #[test]
    fn model_deleted_logs_are_forgotten_and_later_turns_clean_up_normally() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(dir.path()).unwrap());
        let logs = Arc::new(Logs::for_session("test-session"));
        let first_turn = logs.lease();
        let path = logged_output(&logs, &root);
        std::fs::remove_file(dir.path().join(path)).unwrap();
        drop(first_turn);
        assert!(logs.state.lock().unwrap().files.is_empty());
        let second_turn = logs.lease();
        let path = logged_output(&logs, &root);
        drop(second_turn);
        assert!(!dir.path().join(path).exists());
    }

    #[tokio::test]
    async fn a_dropped_task_releases_its_log_lease() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(dir.path()).unwrap());
        let logs = Arc::new(Logs::for_session("test-session"));
        let lease = logs.lease();
        let path = logged_output(&logs, &root);
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _lease = lease;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!dir.path().join(path).exists());
    }

    #[test]
    fn cleanup_uses_the_pinned_log_directory_after_a_path_swap() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(dir.path()).unwrap());
        let logs = Arc::new(Logs::for_session("test-session"));
        let lease = logs.lease();
        let path = logged_output(&logs, &root);
        let name = Path::new(&path).file_name().unwrap();
        let original = dir.path().join(".otto/tool-output");
        let moved = dir.path().join(".otto/moved-output");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join(name), "unrelated replacement").unwrap();
        drop(lease);
        assert!(!moved.join("test-session").join(name).exists());
        assert_eq!(
            std::fs::read_to_string(original.join(name)).unwrap(),
            "unrelated replacement"
        );
    }

    #[test]
    fn overflow_log_path_names_the_owning_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(dir.path()).unwrap());
        let logs = Arc::new(Logs::for_session("session-x"));
        let path = logged_output(&logs, &root);
        let prefix = ".otto/tool-output/session-x/";
        assert!(path.starts_with(prefix) && path.ends_with(".log"), "{path}");
        assert_eq!(&path[prefix.len()..].len(), &36);
        assert!(dir.path().join(&path).is_file());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".otto/tool-output/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(
            std::fs::metadata(dir.path().join(".otto/tool-output/session-x"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn invalid_session_ids_create_no_log_and_no_directories() {
        for id in ["..", "a/b", "", "."] {
            let dir = tempfile::tempdir().unwrap();
            let root = Arc::new(Root::open(dir.path()).unwrap());
            let mut output = Output::with_logs(root, 4, Arc::new(Logs::for_session(id)));
            output.write(b"head complete log tail");
            output.finish();
            assert!(
                output.render("stdout").contains("full_output: unavailable"),
                "{id:?}"
            );
            assert!(!dir.path().join(".otto").exists(), "{id:?}");
        }
    }

    #[test]
    fn short_streams_are_unchanged_and_do_not_create_files() {
        let (dir, mut output) = setup(12);
        output.write(b"all passed\n");
        output.finish();
        assert_eq!(output.render("stdout"), "stdout:\nall passed\n");
        assert!(!dir.path().join(".otto").exists());
    }

    #[test]
    fn overflow_keeps_head_and_final_failure_and_saves_every_byte() {
        let (dir, mut output) = setup(32);
        let text = format!("begin\n{}\nFAILED: 7 tests\n", "test ok\n".repeat(100));
        for chunk in text.as_bytes().chunks(7) {
            output.write(chunk);
        }
        output.finish();
        let result = output.render("stdout");
        assert!(result.starts_with("stdout:\nbegin\n"), "{result}");
        assert!(result.contains("FAILED: 7 tests"), "{result}");
        assert!(result.contains(&format!("output_bytes: {}", text.len())));
        let path = dir.path().join(&output.log.as_ref().unwrap().0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(result.len() < 300);
    }

    #[test]
    fn redaction_precedes_both_preview_and_persistence_across_chunks() {
        let (dir, output) = setup(10);
        let mut stream = RedactingCollector::with_collector(output, &["split-secret".into()], "•");
        for chunk in [
            b"start split-".as_slice(),
            b"secret mid\xff split-sec",
            b"ret end",
        ] {
            stream.write(chunk);
        }
        stream.flush();
        stream.collector_mut().finish();
        let output = stream.collector();
        let saved =
            std::fs::read_to_string(dir.path().join(&output.log.as_ref().unwrap().0)).unwrap();
        assert_eq!(saved, "start • mid\u{fffd} • end");
        let result = output.render("stderr");
        assert!(!result.contains("split-secret"));
        assert!(
            !result.contains('\u{fffd}'),
            "preview split a rune: {result}"
        );
    }

    #[test]
    fn storage_failure_preserves_preview_and_never_claims_a_complete_log() {
        let (dir, mut output) = setup(8);
        std::fs::write(dir.path().join(".otto"), "blocked").unwrap();
        output.write(b"HEAD discarded TAIL");
        output.finish();
        let result = output.render("stdout");
        assert!(result.contains("HEAD"));
        assert!(result.contains("TAIL"));
        assert!(result.contains("full_output: unavailable"));
    }

    #[test]
    fn disk_write_failure_is_reported_without_failing_the_stream() {
        let (_dir, mut output) = setup(8);
        // A read-only descriptor deterministically fails on flush on both hosts.
        output.log = Some((
            "not-a-complete-log".into(),
            BufWriter::new(File::open("/dev/null").unwrap()),
        ));
        output.write(b"HEAD discarded TAIL");
        output.finish();
        assert!(output.render("stdout").contains("full_output: unavailable"));
    }

    #[test]
    fn logs_cannot_follow_a_workspace_escape() {
        let (dir, mut output) = setup(8);
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join(".otto")).unwrap();
        output.write(b"HEAD discarded TAIL");
        output.finish();
        assert!(output.render("stdout").contains("full_output: unavailable"));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn log_directory_cannot_alias_a_protected_workspace_directory() {
        let (dir, mut output) = setup(8);
        std::fs::create_dir(dir.path().join(".otto")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
        symlink("../.git/hooks", dir.path().join(".otto/tool-output")).unwrap();
        output.write(b"HEAD discarded TAIL");
        output.finish();
        assert!(output.render("stdout").contains("full_output: unavailable"));
        assert_eq!(
            std::fs::read_dir(dir.path().join(".git/hooks"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn separate_streams_get_unique_complete_logs() {
        let (dir, mut first) = setup(4);
        let mut second = Output::new(Arc::clone(&first.root), 4);
        first.write(b"first stream");
        second.write(b"second stream");
        first.finish();
        second.finish();
        let a = &first.log.as_ref().unwrap().0;
        let b = &second.log.as_ref().unwrap().0;
        assert_ne!(a, b);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(a)).unwrap(),
            "first stream"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(b)).unwrap(),
            "second stream"
        );
    }
}
