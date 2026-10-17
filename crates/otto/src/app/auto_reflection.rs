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
mod tests {
    use super::*;

    #[test]
    fn notices_are_drained_in_order_and_bounded() {
        let notices = Notices::new();
        let changes = notices.signal.subscribe();
        notices.push("first".into());
        assert!(changes.has_changed().expect("open"));
        notices.push("second".into());
        assert_eq!(notices.take(), ["first", "second"]);
        assert!(notices.take().is_empty());
        for index in 0..MAXIMUM_NOTICES + 5 {
            notices.push(format!("n{index}"));
        }
        let kept = notices.take();
        assert_eq!(kept.len(), MAXIMUM_NOTICES);
        assert_eq!(kept[0], "n5", "the oldest are dropped first");
        let (_, last) = notices.since(0);
        assert_eq!(
            last as usize,
            MAXIMUM_NOTICES + 5 + 2,
            "ids keep counting past the cap"
        );
    }

    #[test]
    fn notices_can_be_read_by_id_without_draining() {
        let notices = Notices::new();
        assert_eq!(notices.since(0), (Vec::new(), 0));
        notices.push("first".into());
        notices.push("second".into());
        let (all, last) = notices.since(0);
        assert_eq!(
            all,
            [
                Notice {
                    id: 1,
                    text: "first".to_owned(),
                    presentation_origin: String::new(),
                },
                Notice {
                    id: 2,
                    text: "second".to_owned(),
                    presentation_origin: String::new(),
                },
            ]
        );
        assert_eq!(last, 2);
        // A second reader sees the same lines; a reader that is caught up sees none.
        assert_eq!(notices.since(0).0.len(), 2);
        assert_eq!(notices.since(2), (Vec::new(), 2));
        assert_eq!(notices.since(1).0[0].text, "second");
        // Draining is for the single-reader frontends and does not reuse ids.
        assert_eq!(notices.take(), ["first", "second"]);
        assert_eq!(notices.since(0), (Vec::new(), 2));
        notices.push("third".into());
        assert_eq!(notices.since(2).0[0].text, "third");
    }

    #[test]
    fn a_completed_compaction_is_noticed_and_a_noop_is_not() {
        use otto_core::agent::CompactionResult;
        let compacted = AtomicBool::new(false);
        let mut seen = 0;
        {
            let mut sink = |_event: Event| seen += 1;
            let mut observed = observing(&mut sink, &compacted);
            observed(Event::AgentStarted);
            observed(Event::CompactionCompleted {
                compaction: CompactionResult {
                    noop: true,
                    ..CompactionResult::default()
                },
            });
            assert!(
                !compacted.load(Ordering::SeqCst),
                "a noop is not a compaction"
            );
            observed(Event::CompactionCompleted {
                compaction: CompactionResult {
                    checkpoint_id: "abc12345".into(),
                    ..CompactionResult::default()
                },
            });
        }
        assert!(compacted.load(Ordering::SeqCst));
        assert_eq!(seen, 3, "every event still reaches the frontend");
    }

    fn finished_tool(
        id: &str,
        attempt: u32,
        outcome: OperationOutcome,
        result: ToolResult,
    ) -> Event {
        Event::ToolCallFinished {
            operation_id: OperationId::new(id).expect("operation id"),
            attempt,
            tool_name: "read".into(),
            tool_call_id: format!("call-{id}-{attempt}"),
            result,
            outcome,
        }
    }

    fn successful_tool(id: &str, attempt: u32) -> Event {
        finished_tool(
            id,
            attempt,
            OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
            ToolResult::default(),
        )
    }

    #[test]
    fn task_end_requires_ten_distinct_successes_and_a_finished_run() {
        let observation = RunObservation::default();
        for index in 0..9 {
            observation.observe(&successful_tool(&format!("op_{index}"), 1));
        }
        // A successful retry of an already counted logical operation is still
        // one operation; failed and canceled calls never count.
        observation.observe(&successful_tool("op_0", 2));
        observation.observe(&finished_tool(
            "op_error",
            1,
            OperationOutcome {
                disposition: OperationDisposition::Error,
                effect_certainty: EffectCertainty::KnownNoEffect,
                stop_reason: None,
            },
            ToolResult::error("no"),
        ));
        observation.observe(&finished_tool(
            "op_cancelled",
            1,
            OperationOutcome {
                disposition: OperationDisposition::Cancelled,
                effect_certainty: EffectCertainty::NotStarted,
                stop_reason: Some(otto_core::model::OperationStopReason::UserCancellation),
            },
            ToolResult::error("cancelled"),
        ));
        assert!(!observation.eligible_work(true));

        observation.observe(&successful_tool("op_9", 1));
        assert!(!observation.eligible_work(true), "the run has not finished");
        observation.observe(&Event::AgentFinished);
        assert!(observation.eligible_work(true));
        assert!(
            !observation.eligible_work(false),
            "errors/cancellation suppress"
        );
    }

    #[test]
    fn approval_required_suppresses_task_end_reflection() {
        let observation = RunObservation::default();
        for index in 0..10 {
            observation.observe(&successful_tool(&format!("op_{index}"), 1));
        }
        observation.observe(&Event::AgentFinished);
        observation.observe(&Event::ToolCallFinished {
            operation_id: OperationId::new("op_approval").expect("operation id"),
            attempt: 1,
            tool_name: "bash".into(),
            tool_call_id: "call-approval".into(),
            result: ToolResult {
                content: "approval required.\nApprove in Otto: /approve approval-1\nCommand: \"git push\"\nJustification: \"push\"".into(),
                is_error: true,
                ..ToolResult::default()
            },
            outcome: OperationOutcome {
                disposition: OperationDisposition::Error,
                effect_certainty: EffectCertainty::NotStarted,
                stop_reason: None,
            },
        });
        assert!(!observation.eligible_work(true));
    }

    #[test]
    fn every_completed_run_and_every_failure_is_announced() {
        let dir = Path::new("/logs");
        let skipped = Report::skipped_for_test();
        assert_eq!(notice_for(Ok(skipped.clone()), Some(dir)), None);
        assert_eq!(notice_for(Err(Error::Cancelled), Some(dir)), None);
        assert_eq!(notice_for(Err(Error::Disabled), Some(dir)), None);
        let mut noop = skipped;
        noop.note.clear();
        noop.log = Some(Ok("/logs/run.md".into()));
        let line = notice_for(Ok(noop), Some(dir)).expect("a noop run is announced");
        assert_eq!(
            line,
            "reflection: nothing new to reflect on; log: /logs/run.md"
        );
        let failure =
            notice_for(Err(Error::Model("boom".into())), Some(dir)).expect("a failure is shown");
        assert_eq!(
            failure,
            "reflection: reflection model call failed: boom; run logs: /logs"
        );
        let failure = notice_for(Err(Error::Model("boom".into())), None).expect("shown");
        assert!(!failure.contains("run logs"), "{failure}");
    }

    // -- end to end: a compaction starts a background run ---------------------

    use std::sync::atomic::AtomicUsize;

    use otto_core::agent::summary::SUMMARIZATION_SYSTEM_PROMPT;
    use otto_core::model::{
        EffectCertainty, FinishReason, Message, OperationDisposition, OperationId,
        OperationOutcome, Role,
    };
    use otto_core::operation::OperationControl;
    use otto_core::provider::{Provider, ProviderSettlement, Request, Response, StreamSink};
    use otto_core::session::Session;
    use otto_core::tool::ToolResult;

    use crate::cli::testutil::{builder, initial_runtime};
    use crate::runtime::builder::Runner;
    use crate::subagent::tasks::Tasks;

    const SUMMARY: &str = "## Goal\nx\n## Constraints & Preferences\nx\n## Observations\nx\n## Progress\n### Done\nx\n### In Progress\nx\n### Blocked\nx\n## Key Decisions\nx\n## Next Steps\nx\n## Critical Context\nx";

    /// Answers a compaction request with a valid summary and a reflection
    /// request with `reflection`, counting each.
    struct Router {
        reflection: String,
        compactions: AtomicUsize,
        reflections: AtomicUsize,
        /// How long a reflection answer takes.
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl Provider for Router {
        async fn complete(
            &self,
            request: &Request,
            _emit: StreamSink<'_>,
            control: &dyn OperationControl,
        ) -> ProviderSettlement {
            let text = if request
                .system_prompt
                .starts_with(SUMMARIZATION_SYSTEM_PROMPT)
            {
                self.compactions.fetch_add(1, Ordering::SeqCst);
                SUMMARY.to_owned()
            } else {
                self.reflections.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    () = tokio::time::sleep(self.delay) => {}
                    () = control.cancellation_token().cancelled() => {
                        return ProviderSettlement::failed(
                            otto_core::provider::ProviderError::Cancelled,
                            1,
                            EffectCertainty::KnownNoEffect,
                        );
                    }
                }
                self.reflection.clone()
            };
            ProviderSettlement::succeeded(
                Response {
                    message: Message {
                        role: Role::Assistant,
                        blocks: vec![otto_core::model::Block::text(text)],
                        finish_reason: Some(FinishReason::Stop),
                        ..Message::default()
                    },
                    cached_input_tokens_present: false,
                    remote_compaction: None,
                },
                1,
            )
        }
    }

    struct Rig {
        controller: Controller,
        provider: Arc<Router>,
        service: Arc<crate::memory::Service>,
        user_scope: crate::memory::Scope,
        _dirs: Vec<tempfile::TempDir>,
    }

    /// A controller over a persisted session of `exchanges` user and assistant
    /// pairs, with a real memory store and a reflector configured by `auto`.
    async fn rig(auto: Auto, exchanges: usize) -> Rig {
        rig_with(auto, exchanges, std::time::Duration::ZERO).await
    }

    async fn rig_with(auto: Auto, exchanges: usize, delay: std::time::Duration) -> Rig {
        use crate::reflection::{Reflector, Store};
        use crate::runtime::wiring::{MemoryWiring, open_memory_service, workspace_memory_scope};
        use otto_core::config::ReflectionRuntime;
        use otto_core::config::memory::MemoryRuntime;

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let memory = tempfile::tempdir().expect("memory");
        let runtime_config = MemoryRuntime {
            enabled: true,
            backend: "files".into(),
            dir: memory.path().join("memory").to_string_lossy().into_owned(),
            ..MemoryRuntime::default()
        };
        let (service, user_scope, usable) =
            open_memory_service(&runtime_config, &[], &mut Vec::new()).expect("memory");
        let mut builder = builder(workspace.path(), sessions.path());
        builder.shared_mut().home = home.path().to_string_lossy().into_owned();
        builder.shared_mut().memory = MemoryWiring {
            client: None,
            extension: None,
            service: Arc::clone(&service),
            usable,
            user_scope: user_scope.clone(),
            recall_limit: 8,
            recall_token_budget: 1000,
            advice_audit: None,
            turns: None,
        };
        builder.workspace_scope =
            workspace_memory_scope(&runtime_config, &workspace.path().to_string_lossy())
                .expect("workspace scope");
        builder.shared_mut().reflector = Arc::new(
            Reflector::new(
                ReflectionRuntime {
                    auto,
                    skills: false,
                    ..ReflectionRuntime::default()
                },
                Some(Arc::new(Store::open_in_memory().expect("store"))),
            )
            .with_log_dir(home.path().join(".otto/reflection/runs")),
        );
        let runtime = initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let mut user_ids = Vec::new();
        for index in 0..exchanges {
            session
                .append(Message {
                    role: Role::User,
                    blocks: vec![otto_core::model::Block::text(format!(
                        "Message {index}: please always answer in Chinese from now on, thanks."
                    ))],
                    created_at: chrono::Utc::now(),
                    ..Message::default()
                })
                .await
                .expect("append user");
            user_ids.push(session.messages().last().expect("user").id.clone());
            session
                .append(Message {
                    role: Role::Assistant,
                    blocks: vec![otto_core::model::Block::text(format!("Reply {index}."))],
                    finish_reason: Some(FinishReason::Stop),
                    created_at: chrono::Utc::now(),
                    ..Message::default()
                })
                .await
                .expect("append assistant");
        }
        let reflection = serde_json::json!({"retrospective": "Asked for Chinese replies; nothing took a detour.", "memories": [{
            "action": "create", "scope": "user", "kind": "preference", "key": "language",
            "text": "Answer in Chinese", "confidence": 0.9, "reason": "the user said so",
            "evidence": [{"entry": user_ids.first().cloned().unwrap_or_default(), "quote": "always answer in Chinese"}],
        }]})
        .to_string();
        let provider = Arc::new(Router {
            reflection,
            compactions: AtomicUsize::new(0),
            reflections: AtomicUsize::new(0),
            delay,
        });
        let runner = Runner::scripted(
            session.clone(),
            Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
            Arc::new(Tasks::new()),
        );
        let info = builder.runtime_info(&runtime);
        Rig {
            controller: Controller::new(builder, true, session, runner, info),
            provider,
            service,
            user_scope,
            _dirs: vec![workspace, sessions, home, memory],
        }
    }

    async fn compact(rig: &Rig) {
        let _ = rig
            .controller
            .compact("", &mut |_event| {}, &CancellationToken::new())
            .await;
    }

    async fn first_notice(rig: &Rig) -> String {
        let mut changes = rig.controller.notices_changed();
        for _ in 0..200 {
            let notices = rig.controller.take_notices();
            if let Some(line) = notices
                .into_iter()
                .find(|line| !line.starts_with("reflection: running"))
            {
                return line;
            }
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(50), changes.changed()).await;
        }
        panic!("no notice arrived");
    }

    fn pending(rig: &Rig) -> usize {
        use crate::memory::{CandidateState, SearchRequest};
        rig.service
            .search(&SearchRequest {
                scopes: vec![rig.user_scope.clone()],
                include_candidates: true,
                candidate_states: vec![CandidateState::Pending],
                limit: 20,
                token_budget: 1,
                now: chrono::Utc::now(),
                ..SearchRequest::default()
            })
            .expect("search")
            .candidates
            .len()
    }

    fn successful_work() -> RunObservation {
        let observation = RunObservation::default();
        for index in 0..MINIMUM_SUCCESSFUL_TOOLS {
            observation.observe(&successful_tool(&format!("work_{index}"), 1));
        }
        observation.observe(&Event::AgentFinished);
        observation
    }

    #[tokio::test]
    async fn on_work_reflects_only_after_qualified_work_and_also_keeps_compaction_trigger() {
        let rig = rig(Auto::OnWork, 6).await;
        let observation = successful_work();
        rig.controller.after_prompt(&observation, true);
        let line = first_notice(&rig).await;
        assert!(line.contains("1 candidate(s) queued for review"), "{line}");
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
        assert_eq!(pending(&rig), 1);

        // The task-end and compaction triggers share one automatic cooldown.
        compact(&rig).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
        assert!(rig.controller.take_notices().is_empty());
    }

    #[tokio::test]
    async fn only_on_work_auto_mode_runs_the_task_end_trigger() {
        for auto in [Auto::Off, Auto::OnExit, Auto::OnCompaction] {
            let rig = rig(auto, 6).await;
            let observation = successful_work();
            rig.controller.after_prompt(&observation, true);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert_eq!(
                rig.provider.reflections.load(Ordering::SeqCst),
                0,
                "{auto:?} must not run task-end reflection"
            );
            assert!(rig.controller.take_notices().is_empty());
        }
    }

    #[tokio::test]
    async fn compaction_modes_start_one_background_reflection_and_queue_a_notice() {
        for auto in [Auto::OnCompaction, Auto::OnWork] {
            let rig = rig(auto, 6).await;
            compact(&rig).await;
            assert_eq!(
                rig.provider.compactions.load(Ordering::SeqCst),
                1,
                "{auto:?}: the compaction ran"
            );

            let line = first_notice(&rig).await;
            assert!(line.contains("1 candidate(s) queued for review"), "{line}");
            assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
            assert_eq!(pending(&rig), 1);

            // A second compaction inside the interval starts nothing.
            compact(&rig).await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
            assert!(rig.controller.take_notices().is_empty());
        }
    }

    #[tokio::test]
    async fn auto_off_and_on_exit_do_not_reflect_after_a_compaction() {
        for auto in [Auto::Off, Auto::OnExit] {
            let rig = rig(auto, 6).await;
            compact(&rig).await;
            assert_eq!(
                rig.provider.compactions.load(Ordering::SeqCst),
                1,
                "{auto:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert_eq!(
                rig.provider.reflections.load(Ordering::SeqCst),
                0,
                "{auto:?}"
            );
            assert!(rig.controller.take_notices().is_empty());
        }
    }

    #[tokio::test]
    async fn a_compaction_that_does_nothing_starts_no_reflection() {
        let rig = rig(Auto::OnCompaction, 0).await;
        compact(&rig).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn on_exit_reflects_once_for_a_long_enough_session() {
        let rig = rig(Auto::OnExit, 5).await;
        assert!(rig.controller.reflects_on_exit());
        let line = rig
            .controller
            .reflect_on_exit(&CancellationToken::new())
            .await
            .expect("something to show");
        assert!(line.contains("1 candidate(s) queued for review"), "{line}");
        assert_eq!(pending(&rig), 1);
        // Nothing new: the watermark covers it.
        assert_eq!(
            rig.controller
                .reflect_on_exit(&CancellationToken::new())
                .await,
            None
        );
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn on_exit_does_nothing_when_another_mode_is_selected_or_the_session_is_short() {
        let rig = rig(Auto::OnCompaction, 5).await;
        assert!(!rig.controller.reflects_on_exit());
        assert_eq!(
            rig.controller
                .reflect_on_exit(&CancellationToken::new())
                .await,
            None
        );
        let short = self::rig(Auto::OnExit, 2).await;
        assert_eq!(
            short
                .controller
                .reflect_on_exit(&CancellationToken::new())
                .await,
            None
        );
        assert_eq!(short.provider.reflections.load(Ordering::SeqCst), 0);
    }

    /// The `reflect` tool of the controller's current runner.
    fn reflect_tool(rig: &Rig) -> crate::tool::reflect::ReflectTool {
        let runner = rig.controller.runner().expect("runner");
        crate::tool::reflect::ReflectTool::new(Arc::clone(&runner.reflection))
    }

    async fn call(
        tool: &crate::tool::reflect::ReflectTool,
        cancel: &CancellationToken,
    ) -> ToolResult {
        use crate::tool::Tool;
        tool.execute(
            &crate::tool::testutil::raw(r#"{"focus":"language"}"#),
            cancel,
        )
        .await
    }

    #[tokio::test]
    async fn the_reflect_tool_runs_a_requested_reflection_whatever_the_auto_mode() {
        let rig = rig(Auto::Off, 3).await;
        let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("1 candidate(s) queued for review"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains(
                "\n\nRetrospective:\nAsked for Chinese replies; nothing took a detour.\n"
            ),
            "{}",
            result.content
        );
        assert_eq!(pending(&rig), 1);
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);

        // Every frontend gets the same line as a notice, and the run log
        // opens with the retrospective.
        let notices = rig.controller.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        let log = notices[0].rsplit("; log: ").next().expect("log path");
        let log = std::fs::read_to_string(log).expect("run log");
        assert!(log.contains("- trigger: requested"), "{log}");
        assert!(
            log.contains("## Retrospective\n\n> Asked for Chinese replies"),
            "{log}"
        );
    }

    #[tokio::test]
    async fn the_reflect_tool_is_rebound_when_the_runner_is_replaced() {
        let rig = rig(Auto::Off, 3).await;
        let session = rig.controller.current_session_opt().expect("session");
        let replaced = Runner::scripted(
            session.clone(),
            Arc::clone(&rig.provider) as Arc<dyn Provider + Send + Sync>,
            Arc::new(Tasks::new()),
        );
        let builder = &rig.controller.builder;
        let info = builder.runtime_info(&initial_runtime(builder));
        assert!(
            rig.controller
                .replace_runner_if_current(&session.header().id, replaced, info)
                .expect("replace")
        );
        let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_reflect_tool_shares_the_single_flight_flag_and_ignores_the_cooldown() {
        let rig = rig(Auto::OnWork, 6).await;
        rig.controller.auto_running.store(true, Ordering::SeqCst);
        let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            super::super::requested_reflection::ALREADY_RUNNING
        );
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 0);
        rig.controller.auto_running.store(false, Ordering::SeqCst);

        // An automatic run just started, and the cooldown would hold back
        // another automatic one; a requested one still runs.
        rig.controller.after_prompt(&successful_work(), true);
        let _ = first_notice(&rig).await;
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
        let session = rig.controller.current_session_opt().expect("session");
        session
            .append(Message {
                role: Role::User,
                blocks: vec![otto_core::model::Block::text(
                    "Please do a retrospective of this session.",
                )],
                created_at: chrono::Utc::now(),
                ..Message::default()
            })
            .await
            .expect("append the request");
        let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 2);
        assert!(!rig.controller.auto_running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelling_the_turn_cancels_a_requested_reflection() {
        let rig = rig_with(Auto::Off, 3, std::time::Duration::from_secs(30)).await;
        let cancel = CancellationToken::new();
        let tool = reflect_tool(&rig);
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            stop.cancel();
        });
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), call(&tool, &cancel))
            .await
            .expect("the cancel ends the run");
        assert!(result.is_error);
        assert!(result.content.contains("cancel"), "{}", result.content);
        assert_eq!(pending(&rig), 0);
        assert!(!rig.controller.auto_running.load(Ordering::SeqCst));
    }
}
