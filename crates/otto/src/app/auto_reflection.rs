//! Automatic reflection: runs that start without a `/reflect`.
//!
//! `on_work` (the default) starts a background run after a successful
//! compaction or a completed prompt that used ten distinct successful tools;
//! `on_compaction` keeps only the first trigger, and `on_exit` runs once as a
//! terminal frontend closes. All go through [`crate::reflection::Reflector`],
//! so they share every check a manual run has, plus one run at a time, a
//! minimum interval per session, `min_turns` for `on_exit`, and no retry.
//!
//! Ownership: a background run owns clones of everything it reads (the runner,
//! the session path, the memory service, the skill roots), so it does not
//! borrow the [`Controller`]. Its result is a one-line notice queued on
//! [`Notices`], which a frontend drains with [`Controller::take_notices`].
//!
//! Concurrency and cancellation: a background run does not take the
//! controller's admission. It reads the session file read-only (a record being
//! appended is ignored until complete) and asks the provider, so it can run
//! beside a turn. At most one runs at a time. `Controller::request_close`
//! cancels it.
//!
//! Errors: a failed run queues one notice; nothing retries it.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::agent::Event;
use otto_core::config::Auto;
use otto_core::model::{EffectCertainty, OperationDisposition, OperationId};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{CLOSED, Controller};
use crate::reflection::{self, Error, Report, Trigger};

/// The most notices kept; the oldest is dropped past it.
const MAXIMUM_NOTICES: usize = 20;
/// A task-end reflection is only useful after substantial tool use.
const MINIMUM_SUCCESSFUL_TOOLS: usize = 10;

/// How long `on_exit` may delay a terminal frontend's exit.
pub const EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
    pub(super) fn push_for(&self, line: String, origin: Option<&crate::schedule::Origin>) {
        self.push_with_origin(line, origin.map_or("", crate::schedule::Origin::as_str));
    }

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

/// Clears the single-flight flag when a background run ends, however it ends.
pub(super) struct Running(pub(super) Arc<AtomicBool>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Default)]
pub(super) struct RunObservation {
    state: Mutex<RunState>,
}

#[derive(Default)]
struct RunState {
    compacted: bool,
    finished: bool,
    approval_required: bool,
    successful_operations: HashSet<OperationId>,
}

impl RunObservation {
    fn observe(&self, event: &Event) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match event {
            Event::AgentFinished => state.finished = true,
            Event::CompactionCompleted { compaction }
                if !compaction.noop && !compaction.checkpoint_id.is_empty() =>
            {
                state.compacted = true;
            }
            Event::ToolCallFinished {
                operation_id,
                tool_name,
                result,
                outcome,
                ..
            } => {
                if crate::tool::bash::parse_approval_request(tool_name, result).is_some() {
                    state.approval_required = true;
                }
                if !result.is_error
                    && outcome.disposition == OperationDisposition::Succeeded
                    && outcome.effect_certainty == EffectCertainty::Completed
                {
                    state.successful_operations.insert(operation_id.clone());
                }
            }
            _ => {}
        }
    }

    pub(super) fn compacted(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .compacted
    }

    pub(super) fn eligible_work(&self, completed: bool) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        completed
            && state.finished
            && !state.approval_required
            && state.successful_operations.len() >= MINIMUM_SUCCESSFUL_TOOLS
    }
}

/// Wraps `emit` so the controller learns whether a compaction completed.
pub(super) fn observing<'a>(
    emit: otto_core::agent::EventSink<'a>,
    compacted: &'a AtomicBool,
) -> impl FnMut(Event) + Send + use<'a> {
    move |event| {
        if let Event::CompactionCompleted { compaction } = &event
            && !compaction.noop
            && !compaction.checkpoint_id.is_empty()
        {
            compacted.store(true, Ordering::SeqCst);
        }
        emit(event);
    }
}

/// Records task completion, approvals, successful operations, and compaction
/// for one provider run. Operation IDs deduplicate retries of the same tool.
pub(super) fn observing_prompt<'a>(
    emit: otto_core::agent::EventSink<'a>,
    observation: &'a RunObservation,
) -> impl FnMut(Event) + Send + use<'a> {
    move |event| {
        observation.observe(&event);
        emit(event);
    }
}

impl Controller {
    /// Takes the lines background reflection has queued since the last call.
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

    /// Queues a notice as background reflection would, for frontend tests.
    #[cfg(test)]
    pub(crate) fn push_notice(&self, line: &str) {
        self.notices.push(line.to_owned());
    }

    #[cfg(test)]
    pub(crate) fn push_routed_notice(&self, line: &str, origin: crate::schedule::Origin) {
        self.notices.push_for(line.to_owned(), Some(&origin));
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

    /// Starts a background reflection when its configured trigger is enabled.
    /// Never blocks or fails the operation that triggered it.
    pub(super) fn start_auto_reflection(&self, trigger: Trigger) {
        let reflector = Arc::clone(&self.builder.reflector);
        let enabled = match trigger {
            Trigger::OnCompaction => {
                matches!(reflector.auto(), Auto::OnCompaction | Auto::OnWork)
            }
            Trigger::OnWork => reflector.auto() == Auto::OnWork,
            Trigger::Manual | Trigger::Requested | Trigger::OnExit => false,
        };
        if !enabled || !self.dynamic_content {
            return;
        }
        let Some(session) = self.current_session_opt() else {
            return;
        };
        let session_path = session.path();
        if session_path.is_empty() {
            return;
        }
        let session_id = session.header().id;
        if reflector.too_soon(&session_id) {
            return;
        }
        let Ok(runner) = self.runner() else {
            return;
        };
        let Some((service, user_scope, workspace_scope)) = self.memory_manager() else {
            return;
        };
        let skill_roots = self.reflection_skill_roots();
        if self.auto_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let running = Running(Arc::clone(&self.auto_running));
        let notices = Arc::clone(&self.notices);
        let cancel = self.auto_cancel.child_token();
        let approvals = self.builder.approvals.clone();
        let presentation_origin = runner.presentation_origin.get();
        notices.push_for(
            format!("reflection: running ({})", trigger.as_str()),
            presentation_origin.as_ref(),
        );
        tokio::spawn(async move {
            let _running = running;
            let context = reflection::Context {
                approvals: Some(&approvals),
                runner: &runner,
                session_id: &session_id,
                presentation_origin: presentation_origin.as_ref(),
                session_path: &session_path,
                service: &service,
                user_scope: &user_scope,
                workspace_scope: &workspace_scope,
                skill_roots: skill_roots.as_ref(),
            };
            let result = reflector.run(&context, trigger, "", &cancel).await;
            if let Some(line) = notice_for(result, reflector.log_dir()) {
                notices.push_for(line, presentation_origin.as_ref());
            }
        });
    }

    /// Runs `on_exit` reflection and returns the line to show, if any. For a
    /// terminal frontend to call once, after its loop ends normally and
    /// before the controller closes. Bounded by `cancel`; the caller applies
    /// [`EXIT_TIMEOUT`].
    pub async fn reflect_on_exit(&self, cancel: &CancellationToken) -> Option<String> {
        if self.builder.reflector.auto() != Auto::OnExit {
            return None;
        }
        notice_for(
            self.reflect_as(Trigger::OnExit, "", cancel).await,
            self.builder.reflector.log_dir(),
        )
    }

    /// Whether `reflect_on_exit` would do anything, so a frontend can avoid
    /// announcing a wait it will not have.
    pub fn reflects_on_exit(&self) -> bool {
        self.builder.reflector.auto() == Auto::OnExit
    }

    /// Reflects on the current session as `trigger`: the shared body of
    /// `/reflect` and `on_exit`. Takes the same admission as a turn or a
    /// compaction, so it never runs beside either.
    pub(super) async fn reflect_as(
        &self,
        trigger: Trigger,
        focus: &str,
        cancel: &CancellationToken,
    ) -> Result<Report, Error> {
        let _admission = self.begin_operation().map_err(Error::Read)?;
        let runner = self.runner().map_err(Error::Read)?;
        let session = self
            .current_session_opt()
            .ok_or_else(|| Error::Read(CLOSED.to_owned()))?;
        let (service, user_scope, workspace_scope) =
            self.memory_manager().ok_or(Error::MemoryUnavailable)?;
        let session_id = session.header().id;
        let session_path = session.path();
        let skill_roots = self.reflection_skill_roots();
        let presentation_origin = runner.presentation_origin.get();
        let context = reflection::Context {
            approvals: Some(&self.builder.approvals),
            runner: &runner,
            session_id: &session_id,
            presentation_origin: presentation_origin.as_ref(),
            session_path: &session_path,
            service: &service,
            user_scope: &user_scope,
            workspace_scope: &workspace_scope,
            skill_roots: skill_roots.as_ref(),
        };
        self.builder
            .reflector
            .run(&context, trigger, focus, cancel)
            .await
    }
}

/// The line an automatic run produces: the report line of every completed
/// run (it names the run log when one was written), or the failure text with
/// the log directory. Nothing for a skipped run, a canceled run, or a run
/// that reflection is disabled for.
fn notice_for(result: Result<Report, Error>, log_dir: Option<&Path>) -> Option<String> {
    match result {
        Ok(report) if report.is_skipped() => None,
        Ok(report) => Some(report.line()),
        Err(Error::Cancelled | Error::Disabled | Error::NoSession) => None,
        Err(error) => {
            let mut line = format!("reflection: {error}");
            if let Some(dir) = log_dir {
                line.push_str(&format!("; run logs: {}", dir.display()));
            }
            Some(line)
        }
    }
}

#[cfg(test)]
mod tests;
