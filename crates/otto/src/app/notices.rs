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

#[derive(Default)]
struct Queue {
    lines: VecDeque<Notice>,
    last_id: u64,
}

impl Notice {
    fn local_text(self) -> String {
        self.text
    }
}

impl Notices {
    fn push_with_origin(&self, line: String, presentation_origin: &str) {
        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            if queue.lines.len() >= MAXIMUM_NOTICES {
                queue.lines.pop_front();
            }
            queue.last_id += 1;
            let id = queue.last_id;
            queue.lines.push_back(Notice {
                id,
                text: line,
                presentation_origin: presentation_origin.to_owned(),
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
        self.push_with_origin(line, "");
    }

    /// Removes and returns every queued line. For a frontend that is the only
    /// reader (the REPL and the TUI).
    fn take(&self) -> Vec<String> {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .lines
            .drain(..)
            .map(Notice::local_text)
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
            .filter(|notice| notice.id > after)
            .cloned()
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
    pub fn push_scheduled(&self, text: &str) {
        self.notices.push(text.to_owned());
        self.bell.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a scheduled delivery arrived since the last call.
    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// The notices queued after `after` (0 for all still kept) with their ids,
    /// and the newest id so far. Does not remove them: for readers that share
    /// a controller, such as the HTTP server's clients. The REPL and the TUI
    /// use [`Controller::take_notices`] instead, which drains.
    pub fn notices_since(&self, after: u64) -> (Vec<(u64, String)>, u64) {
        let (notices, last) = self.notices.since(after);
        (
            notices
                .into_iter()
                .map(|notice| (notice.id, notice.text))
                .collect(),
            last,
        )
    }

    pub(crate) fn routed_notices_since(&self, after: u64) -> (Vec<Notice>, u64) {
        self.notices.since(after)
    }

    /// Queues a notice as background operations would, for frontend tests.
    #[cfg(test)]
    pub(crate) fn push_notice(&self, line: &str) {
        self.notices.push(line.to_owned());
    }

    /// Queues a wake reply as a finished wake turn would, for server tests.
    #[cfg(test)]
    pub(crate) fn push_wake_reply(&self, text: &str) {
        self.wake_replies.push(text.to_owned());
    }

    /// A signal that changes whenever [`Controller::take_notices`] has
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
