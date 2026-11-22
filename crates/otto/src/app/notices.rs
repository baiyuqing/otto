//! Queued frontend notices and wake replies.
use super::Controller;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::watch;
const MAXIMUM_NOTICES: usize = 20;
/// Lines a background run wants a frontend to show, with a signal that fires
/// when one is added.
pub(super) struct Notices {
    queue: Mutex<Queue>,
    signal: watch::Sender<u64>,
}

/// The queued lines, each with the id it was given when queued. Ids start at 1
/// and only grow, so a reader that remembers the last id it saw can ask for
/// what came after without draining the queue for other readers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub id: u64,
    pub text: String,
    pub presentation_origin: String,
}

/// Which frontends read a queued line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Audience {
    /// Every reader.
    Everyone,
    /// The terminal UI and the line REPL, which take the queue with
    /// [`Controller::take_notices`]. Readers that follow it by id
    /// ([`Controller::routed_notices_since`]) skip it: they receive the same
    /// fact through [`Controller::wake_replies_since`].
    Terminal,
}

struct Entry {
    notice: Notice,
    audience: Audience,
}

#[derive(Default)]
struct Queue {
    lines: VecDeque<Entry>,
    last_id: u64,
}

impl Notice {
    fn local_text(self) -> String {
        self.text
    }
}

impl Notices {
    fn push_with_origin(&self, line: String, presentation_origin: &str, audience: Audience) {
        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            if queue.lines.len() >= MAXIMUM_NOTICES {
                queue.lines.pop_front();
            }
            queue.last_id += 1;
            let id = queue.last_id;
            queue.lines.push_back(Entry {
                notice: Notice {
                    id,
                    text: line,
                    presentation_origin: presentation_origin.to_owned(),
                },
                audience,
            });
        }
        self.signal.send_modify(|count| *count += 1);
    }
}

impl Notices {
    pub(super) fn new() -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            signal: watch::channel(0).0,
        }
    }

    pub(super) fn push(&self, line: String) {
        self.push_with_origin(line, "", Audience::Everyone);
    }

    fn push_terminal(&self, line: String) {
        self.push_with_origin(line, "", Audience::Terminal);
    }

    /// Removes and returns every queued line. For a frontend that is the only
    /// reader (the REPL and the TUI).
    fn take(&self) -> Vec<String> {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .lines
            .drain(..)
            .map(|entry| entry.notice.local_text())
            .collect()
    }

    /// The queued lines with an id above `after`, oldest first, and the id of
    /// the newest line ever queued. Leaves the queue as it is, so several
    /// readers each see every line.
    fn since(&self, after: u64) -> (Vec<Notice>, u64) {
        let queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let lines = queue
            .lines
            .iter()
            .filter(|entry| entry.notice.id > after && entry.audience == Audience::Everyone)
            .map(|entry| entry.notice.clone())
            .collect();
        (lines, queue.last_id)
    }
}

impl Controller {
    /// Takes the lines background operations has queued since the last call.
    pub fn take_notices(&self) -> Vec<String> {
        self.notices.take()
    }

    /// Queues the text of a scheduled delivery as a notice and marks the
    /// terminal bell pending.
    pub(crate) fn push_scheduled(&self, text: &str) {
        self.notices.push(text.to_owned());
        self.bell.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a scheduled delivery arrived since the last call.
    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn routed_notices_since(&self, after: u64) -> (Vec<Notice>, u64) {
        self.notices.since(after)
    }

    /// Tells the user once that notifications are held because a limit of the
    /// last root turn is used up, in every frontend. `reported` is `Some` only
    /// for the call that wrote the `turn_budget_wake_skipped` record of the
    /// root, so a root is announced once. The terminal UI and the line REPL
    /// show it as a notice; the HTTP server and the ACP frontends deliver the
    /// same text through the wake replies, the one out-of-turn text channel
    /// they read.
    pub(super) fn announce_withheld_wake(
        &self,
        withheld: &otto_core::agent::budget::Withheld,
        reported: Option<usize>,
    ) {
        let Some(pending) = reported else { return };
        let text = super::WithheldWake {
            pending,
            exceeded: withheld.exceeded,
        }
        .to_string();
        self.notices.push_terminal(text.clone());
        self.wake_replies.push(text);
    }

    /// Queues a notice as background operations would, for frontend tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn push_notice(&self, line: &str) {
        self.notices.push(line.to_owned());
    }

    /// Queues a wake reply as a finished wake turn would, for server tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn push_wake_reply(&self, text: &str) {
        self.wake_replies.push(text.to_owned());
    }

    /// A signal that changes whenever `Controller::take_notices` has
    /// something new, for frontends that wait on events.
    pub fn notices_changed(&self) -> watch::Receiver<u64> {
        self.notices.signal.subscribe()
    }

    /// The wake-turn replies queued after `after`, with ids, and the newest id
    /// so far. Only ACP and the HTTP server read this queue; the TUI and the
    /// REPL show a wake turn's events directly.
    pub fn wake_replies_since(&self, after: u64) -> (Vec<(u64, String)>, u64) {
        let (replies, last) = self.wake_replies.since(after);
        (
            replies
                .into_iter()
                .map(|reply| (reply.id, reply.text))
                .collect(),
            last,
        )
    }

    /// A signal that changes whenever a wake reply is queued.
    pub fn wake_replies_changed(&self) -> watch::Receiver<u64> {
        self.wake_replies.signal.subscribe()
    }
}
