//! The durable [`Mailbox`](otto_core::mailbox::Mailbox) adapter: an
//! in-memory [`Inbox`] whose every change is mirrored to a JSON sidecar file
//! beside a session or child transcript.
//!
//! [`install`] gives the inbox an input-id source and a persistence hook;
//! [`load`] restores a sidecar before the inbox is shared with any producer.
//! Writes use [`remind::replace_private_with`] (temporary file, `fsync`, rename,
//! directory `fsync`) and report failures to the enqueueing caller instead of
//! dropping them. The sidecar is the pending queue; the session file is the
//! delivery record, which [`Session::append_input_if_absent`] consults so an
//! entry appended before an interruption is not delivered twice.
//!
//! [`Session::append_input_if_absent`]: otto_core::session::Session::append_input_if_absent

use std::path::{Path, PathBuf};

use otto_core::agent::inbox::{Entry, Inbox};

use crate::fsops::{PosixPublish, PublishOps};
use crate::tool::remind;

/// A fresh input id for a notification enqueued without one: `in_` followed
/// by 32 random hex digits, or empty when randomness is unavailable (the
/// notification is then delivered at least once).
pub fn new_input_id() -> String {
    super::builder::random_id()
        .map(|random| format!("in_{random}"))
        .unwrap_or_default()
}

/// Makes `inbox` durable: notifications enqueued without an id get one, and
/// every change is written to the path `sidecar` returns at that moment.
/// `sidecar` returning `None` (no session file yet, or `--no-session`) skips
/// the write, as for a lazy session that has not been created on disk yet.
/// `suffix` is the sidecar's file-name suffix (`.inbox.json`), used to skip a
/// write once the owning transcript is archived.
pub(crate) fn install(
    inbox: &Inbox,
    suffix: &str,
    sidecar: impl Fn() -> Option<PathBuf> + Send + Sync + 'static,
) {
    let suffix = suffix.to_owned();
    inbox.set_id_source(Box::new(new_input_id));
    inbox.set_persist(Box::new(move |entries| match sidecar() {
        Some(path) => write(&path, &suffix, entries),
        None => Ok(()),
    }));
}

/// Restores the sidecar at `path` into `inbox`. A missing file loads an
/// empty queue. An unreadable or malformed file is moved aside to
/// `<path>.corrupt-<unix seconds>` and returned as a warning, rather than
/// silently dropping what it held. Entries written before input ids existed
/// get a fresh id, so they are delivered at least once and exactly once
/// afterwards.
pub(crate) fn load(inbox: &Inbox, path: &Path) -> Option<String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            inbox.load(Vec::new());
            return None;
        }
        Err(error) => {
            inbox.load(Vec::new());
            return Some(set_aside(path, &error.to_string()));
        }
    };
    match serde_json::from_str::<Vec<Entry>>(&raw) {
        Ok(mut entries) => {
            for entry in &mut entries {
                if entry.notification.id.is_empty() {
                    entry.notification.id = new_input_id();
                }
            }
            inbox.load(entries);
            None
        }
        Err(error) => {
            inbox.load(Vec::new());
            Some(set_aside(path, &error.to_string()))
        }
    }
}

fn set_aside(path: &Path, reason: &str) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".corrupt-{seconds}"));
    let aside = PathBuf::from(aside);
    match std::fs::rename(path, &aside) {
        Ok(()) => format!(
            "pending inputs in {} could not be read ({reason}); moved to {}",
            path.display(),
            aside.display()
        ),
        Err(error) => format!(
            "pending inputs in {} could not be read ({reason}) or moved aside: {error}",
            path.display()
        ),
    }
}

/// Mirrors `entries` to `path`, removing the file once the queue is empty.
/// Skips the write once the owning transcript is gone
/// ([`remind::session_file_exists`]): a notification pushed after the
/// session is archived must not recreate the sidecar the archive removed.
pub(crate) fn write(path: &Path, suffix: &str, entries: &[Entry]) -> Result<(), String> {
    write_with(path, suffix, entries, &PosixPublish)
}

fn write_with(
    path: &Path,
    suffix: &str,
    entries: &[Entry],
    publish: &dyn PublishOps,
) -> Result<(), String> {
    if entries.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove {}: {error}", path.display())),
        };
    }
    if !remind::session_file_exists(path, suffix) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(entries)
        .map_err(|error| format!("encode pending inputs: {error}"))?;
    remind::replace_private_with(path, &body, publish)
}

#[cfg(test)]
mod tests {
    use otto_core::agent::inbox::Notification;

    use super::*;
    use crate::faults::publish::FaultyPublishOps;
    use crate::faults::{Fault, FaultPlan, Rule, Trigger};
    use crate::fsops::PublishStep;

    fn session_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("s1.jsonl"), "").expect("session file");
        let path = dir.path().join("s1.inbox.json");
        (dir, path)
    }

    #[test]
    fn the_sidecar_adapter_keeps_the_mailbox_contract_across_a_reload() {
        let root = tempfile::tempdir().expect("dir");
        let next = std::sync::atomic::AtomicUsize::new(0);
        let last = std::sync::Mutex::new(PathBuf::new());
        let fresh = || -> std::sync::Arc<dyn otto_core::mailbox::Mailbox> {
            let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::fs::write(root.path().join(format!("s{n}.jsonl")), "").expect("session file");
            let path = root.path().join(format!("s{n}.inbox.json"));
            *last.lock().expect("path") = path.clone();
            let inbox = Inbox::new(None);
            install(&inbox, ".inbox.json", move || Some(path.clone()));
            assert_eq!(load(&inbox, &last.lock().expect("path")), None);
            std::sync::Arc::new(inbox)
        };
        let reopen = |old: std::sync::Arc<dyn otto_core::mailbox::Mailbox>| {
            drop(old);
            let path = last.lock().expect("path").clone();
            let inbox = Inbox::new(None);
            assert_eq!(load(&inbox, &path), None);
            let hook = path.clone();
            install(&inbox, ".inbox.json", move || Some(hook.clone()));
            std::sync::Arc::new(inbox) as std::sync::Arc<dyn otto_core::mailbox::Mailbox>
        };
        otto_core::conformance::mailbox(&fresh, Some(&reopen))
            .unwrap_or_else(|failure| panic!("{failure}"));
    }

    #[test]
    fn enqueued_notifications_get_an_id_and_reach_the_sidecar() {
        let (_dir, path) = session_dir();
        let inbox = Inbox::new(None);
        let hook_path = path.clone();
        install(&inbox, ".inbox.json", move || Some(hook_path.clone()));
        inbox
            .enqueue(Notification {
                text: "hello".into(),
                ..Notification::default()
            })
            .expect("persisted");
        let stored: Vec<Entry> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("sidecar")).expect("json");
        assert_eq!(stored.len(), 1);
        assert!(stored[0].notification.id.starts_with("in_"));
    }

    /// Two notifications are enqueued through a sidecar whose publish ends
    /// the process at the second save's `step`.
    fn crash_on_second_save(dir: &Path, step: PublishStep) {
        let plan = FaultPlan::new(vec![Rule::new(step, Trigger::Nth(2), Fault::Exit)]);
        let publish = FaultyPublishOps::new(plan);
        std::fs::write(dir.join("s1.jsonl"), "").expect("session file");
        let path = dir.join("s1.inbox.json");
        let inbox = Inbox::new(None);
        inbox.set_id_source(Box::new(new_input_id));
        inbox.set_persist(Box::new(move |entries| {
            write_with(&path, ".inbox.json", entries, &publish)
        }));
        for text in ["previous", "prompt"] {
            let _ = inbox.enqueue(Notification {
                text: text.into(),
                ..Notification::default()
            });
        }
    }

    fn reloaded_texts(dir: &Path) -> Vec<String> {
        let inbox = Inbox::new(None);
        assert_eq!(load(&inbox, &dir.join("s1.inbox.json")), None);
        inbox
            .queued()
            .into_iter()
            .map(|entry| entry.notification.text)
            .collect()
    }

    #[test]
    fn a_crash_before_the_sidecar_rename_reloads_the_previous_queue() {
        use crate::faults::crash::{self, CRASH_EXIT, Role};
        crash::run(crash::name_of(&|| {}), |role| match role {
            Role::Child(dir) => crash_on_second_save(dir, PublishStep::Rename),
            Role::Parent(dir, status) => {
                assert_eq!(
                    status.code(),
                    Some(CRASH_EXIT),
                    "child did not crash at the fault"
                );
                assert_eq!(reloaded_texts(dir), ["previous"]);
            }
        });
    }

    #[test]
    fn a_crash_before_the_sidecar_directory_sync_reloads_the_previous_queue_and_the_prompt() {
        use crate::faults::crash::{self, CRASH_EXIT, Role};
        crash::run(crash::name_of(&|| {}), |role| match role {
            Role::Child(dir) => crash_on_second_save(dir, PublishStep::DirSync),
            Role::Parent(dir, status) => {
                assert_eq!(
                    status.code(),
                    Some(CRASH_EXIT),
                    "child did not crash at the fault"
                );
                assert_eq!(reloaded_texts(dir), ["previous", "prompt"]);
            }
        });
    }

    #[test]
    fn a_failed_write_is_reported_and_the_entry_stays_queued() {
        let (dir, _path) = session_dir();
        // A directory where the sidecar file should be makes the rename fail.
        let blocked = dir.path().join("s1.inbox.json");
        std::fs::create_dir(&blocked).expect("block");
        std::fs::write(blocked.join("x"), "").expect("non-empty dir");
        let inbox = Inbox::new(None);
        install(&inbox, ".inbox.json", move || Some(blocked.clone()));
        let error = inbox
            .enqueue(Notification {
                text: "hello".into(),
                ..Notification::default()
            })
            .expect_err("not persisted");
        let otto_core::agent::inbox::MailboxError::NotPersisted { seq, .. } = error;
        assert_eq!(inbox.queued()[0].seq, seq);
    }

    #[test]
    fn a_corrupt_sidecar_is_moved_aside_with_a_warning() {
        let (dir, path) = session_dir();
        std::fs::write(&path, "{not json").expect("corrupt");
        let inbox = Inbox::new(None);
        let warning = load(&inbox, &path).expect("warning");
        assert!(warning.contains("moved to"), "{warning}");
        assert!(inbox.is_empty());
        assert!(!path.exists());
        let aside = std::fs::read_dir(dir.path())
            .expect("dir")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(aside);
    }

    #[test]
    fn legacy_entries_without_an_id_get_one_on_load() {
        let (_dir, path) = session_dir();
        std::fs::write(
            &path,
            r#"[{"seq":3,"task_id":"t1","kind":"task_finished","text":"done","usage":null}]"#,
        )
        .expect("legacy sidecar");
        let inbox = Inbox::new(None);
        assert_eq!(load(&inbox, &path), None);
        let queued = inbox.queued();
        assert_eq!(queued[0].seq, 3);
        assert!(queued[0].notification.id.starts_with("in_"));
    }

    /// Consumers depend on the `Mailbox` port. Only the places that create
    /// the in-memory adapter name `Inbox` outside tests: this composition
    /// layer, the task registry that owns the parent and child queues, and
    /// the runner's fallback for a task it cannot find.
    #[test]
    fn only_composition_code_names_the_concrete_inbox() {
        const ALLOWED: &[&str] = &[
            "runtime/",
            "subagent/tasks.rs",
            "subagent/runner.rs",
            "subagent/runner/child_run.rs",
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src").flatten() {
                let path = entry.path();
                let name = path.file_name().unwrap().to_string_lossy();
                // Out-of-line test modules: `tests`, `tests.rs`, `*_tests`, `*_tests.rs`.
                let stem = name.trim_end_matches(".rs");
                if stem == "tests" || stem.ends_with("_tests") {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let relative = path
                    .strip_prefix(&root)
                    .expect("under src")
                    .to_string_lossy()
                    .replace('\\', "/");
                if ALLOWED.iter().any(|allowed| relative.starts_with(allowed)) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                let production = text.split("#[cfg(test)]").next().unwrap_or_default();
                if production
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("//"))
                    .any(|line| line.contains("Inbox"))
                {
                    offenders.push(relative);
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "depend on otto_core::mailbox::Mailbox instead of Inbox in {offenders:?}"
        );
    }
}
