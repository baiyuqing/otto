//! Collects the sandbox's denial events for one driver from `log stream`.
//!
//! Ownership: a [`Monitor`] owns one `/usr/bin/log stream` child and a reader
//! task. [`Monitor::close`] kills the child; dropping the monitor does too.
//!
//! Concurrency: the reader task is the only writer of the bounded buffer;
//! readers lock it briefly. Entries carry their receive time, and callers
//! select a time window, so denials of concurrent commands in one driver are
//! not told apart.
//!
//! Errors: there is none to report. When `log` cannot start, `start` returns
//! `None`; when its output ends or fails, `running` turns false and the driver
//! reports no denials.

use std::collections::{HashSet, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::sandbox::Denial;
use crate::sandbox::seatbelt::state::LEAF_PREFIX;

const LOG_PATH: &str = "/usr/bin/log";
/// Entries kept; the oldest are dropped first.
const MAX_ENTRIES: usize = 256;
/// Unique denials reported per command; the rest are counted.
pub(crate) const MAX_REPORTED: usize = 20;

type Entries = Arc<Mutex<VecDeque<(Instant, Denial)>>>;

pub(crate) struct Monitor {
    child: Mutex<Child>,
    entries: Entries,
    running: Arc<AtomicBool>,
}

impl Monitor {
    /// Starts `log stream` filtered on `tag`. Returns `None` when `tag` is not
    /// `otto-sandbox-` followed by hex digits (nothing else may reach the
    /// predicate) or when `log` cannot be spawned. Does not wait until the
    /// stream is ready. Must run inside a Tokio runtime.
    pub(crate) fn start(tag: &str) -> Option<Self> {
        if !valid_tag(tag) {
            return None;
        }
        let mut child = Command::new(LOG_PATH)
            .args(["stream", "--style", "ndjson", "--predicate"])
            .arg(format!("eventMessage CONTAINS \"{tag}\""))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let entries = Entries::default();
        let running = Arc::new(AtomicBool::new(true));
        let (task_entries, task_running, tag) = (entries.clone(), running.clone(), tag.to_owned());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(denial) = parse_line(&line, &tag) {
                    let mut entries = task_entries.lock().unwrap_or_else(|p| p.into_inner());
                    if entries.len() == MAX_ENTRIES {
                        entries.pop_front();
                    }
                    entries.push_back((Instant::now(), denial));
                }
            }
            task_running.store(false, Ordering::SeqCst);
        });
        Some(Self {
            child: Mutex::new(child),
            entries,
            running,
        })
    }

    /// Whether the reader is still receiving output.
    pub(crate) fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// The denials received at or after `since`, oldest first.
    pub(crate) fn since(&self, since: Instant) -> Vec<Denial> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .iter()
            .filter(|(at, _)| *at >= since)
            .map(|(_, denial)| denial.clone())
            .collect()
    }

    pub(crate) fn close(&self) {
        let _ = self
            .child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .start_kill();
    }
}

fn valid_tag(tag: &str) -> bool {
    tag.strip_prefix(LEAF_PREFIX)
        .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Parses one `log stream` output line. `None` for a line that is not JSON, has
/// no string `eventMessage`, lacks `tag`, or is not a sandbox deny message.
fn parse_line(line: &str, tag: &str) -> Option<Denial> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    parse_message(value.get("eventMessage")?.as_str()?, tag)
}

/// The first line of the message is
/// `Sandbox: <process>(<pid>) deny(<n>) <operation> <target>`; the target is
/// the rest of the line and may be empty or contain spaces.
fn parse_message(message: &str, tag: &str) -> Option<Denial> {
    if !message.contains(tag) {
        return None;
    }
    let first = message.lines().next()?.strip_prefix("Sandbox: ")?;
    let mut parts = first.splitn(4, ' ');
    let _process = parts.next()?;
    if !parts.next()?.starts_with("deny(") {
        return None;
    }
    let operation = parts.next()?.to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    Some(Denial { operation, target })
}

/// Removes duplicates (same operation and target), keeping first-seen order,
/// then keeps the first [`MAX_REPORTED`] and counts the rest.
pub(crate) fn summarize(entries: Vec<Denial>) -> (Vec<Denial>, usize) {
    let mut seen = HashSet::new();
    let mut unique: Vec<Denial> = entries
        .into_iter()
        .filter(|denial| seen.insert((denial.operation.clone(), denial.target.clone())))
        .collect();
    let omitted = unique.len().saturating_sub(MAX_REPORTED);
    unique.truncate(MAX_REPORTED);
    (unique, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = "otto-sandbox-0123456789abcdef0123456789abcdef";

    fn event(message: &str) -> String {
        serde_json::json!({ "eventMessage": message }).to_string()
    }

    fn denial(operation: &str, target: &str) -> Denial {
        Denial {
            operation: operation.to_owned(),
            target: target.to_owned(),
        }
    }

    #[test]
    fn parses_the_recorded_probe_event() {
        let line = event(&format!(
            "Sandbox: bash(38406) deny(1) file-write-create /private/tmp/otto-probe.D0NRhk/ws/.git/hooks/pre-commit\n{TAG}"
        ));
        assert_eq!(
            parse_line(&line, TAG),
            Some(denial(
                "file-write-create",
                "/private/tmp/otto-probe.D0NRhk/ws/.git/hooks/pre-commit"
            ))
        );
    }

    #[test]
    fn keeps_spaces_in_the_target_and_parses_network_targets() {
        let spaced = event(&format!(
            "Sandbox: cat(1) deny(1) file-read-data /Users/me/My Files/a b.txt\n{TAG}"
        ));
        assert_eq!(
            parse_line(&spaced, TAG),
            Some(denial("file-read-data", "/Users/me/My Files/a b.txt"))
        );
        let network = event(&format!(
            "Sandbox: curl(2) deny(1) network-outbound 1.2.3.4:443\n{TAG}"
        ));
        assert_eq!(
            parse_line(&network, TAG),
            Some(denial("network-outbound", "1.2.3.4:443"))
        );
        let empty = event(&format!("Sandbox: x(3) deny(1) lsopen\n{TAG}"));
        assert_eq!(parse_line(&empty, TAG), Some(denial("lsopen", "")));
    }

    #[test]
    fn skips_lines_that_are_not_denial_events() {
        let header =
            format!("Filtering the log data using \"eventMessage CONTAINS \\\"{TAG}\\\"\"");
        assert_eq!(parse_line(&header, TAG), None);
        assert_eq!(parse_line("{\"other\":1}", TAG), None);
        assert_eq!(parse_line("{\"eventMessage\":5}", TAG), None);
        let other_tag = event("Sandbox: bash(1) deny(1) file-read-data /x\nother-tag");
        assert_eq!(parse_line(&other_tag, TAG), None);
        let not_deny = event(&format!(
            "Sandbox: bash(1) allow(1) file-read-data /x\n{TAG}"
        ));
        assert_eq!(parse_line(&not_deny, TAG), None);
    }

    #[test]
    fn only_generated_tags_may_reach_the_predicate() {
        assert!(valid_tag(TAG));
        for bad in [
            "",
            "otto-sandbox-",
            "otto-sandbox-xyz",
            "x\" OR \"1",
            "otto-sandbox-ab\"",
        ] {
            assert!(!valid_tag(bad), "{bad}");
        }
    }

    #[test]
    fn summarize_deduplicates_and_caps_at_twenty() {
        let mut entries = Vec::new();
        for i in 0..25 {
            entries.push(denial("file-read-data", &format!("/p/{i}")));
            entries.push(denial("file-read-data", &format!("/p/{i}")));
        }
        entries.push(denial("file-write-data", "/p/0"));
        let (kept, omitted) = summarize(entries);
        assert_eq!(kept.len(), 20);
        assert_eq!(omitted, 6);
        assert_eq!(kept[0], denial("file-read-data", "/p/0"));
        assert_eq!(kept[19], denial("file-read-data", "/p/19"));
    }

    #[test]
    fn summarize_reports_exactly_25_unique_as_20_and_5() {
        let entries = (0..25)
            .flat_map(|i| [denial("op", &i.to_string()), denial("op", &i.to_string())])
            .collect();
        assert_eq!(summarize(entries).1, 5);
    }
}
