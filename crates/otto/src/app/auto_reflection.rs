//! Automatic reflection: runs that start without a `/reflect`.
//!
//! `on_compaction` (the default) starts a background run after a compaction
//! completes; `on_exit` runs once as a terminal frontend closes. Both go
//! through [`crate::reflection::Reflector`], so they share every check a
//! manual run has, plus the limits that only apply here: one run at a time,
//! a minimum interval per session, `min_turns` for `on_exit`, and no retry.
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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use otto_core::agent::Event;
use otto_core::config::Auto;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{CLOSED, Controller};
use crate::reflection::{self, Error, Report, Trigger};

/// The most notices kept; the oldest is dropped past it.
const MAXIMUM_NOTICES: usize = 20;

/// How long `on_exit` may delay a terminal frontend's exit.
pub const EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Lines a background run wants a frontend to show, with a signal that fires
/// when one is added.
pub(super) struct Notices {
    queue: Mutex<VecDeque<String>>,
    signal: watch::Sender<u64>,
}

impl Notices {
    pub(super) fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            signal: watch::channel(0).0,
        }
    }

    fn push(&self, line: String) {
        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            if queue.len() >= MAXIMUM_NOTICES {
                queue.pop_front();
            }
            queue.push_back(line);
        }
        self.signal.send_modify(|count| *count += 1);
    }

    fn take(&self) -> Vec<String> {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }
}

/// Clears the single-flight flag when a background run ends, however it ends.
struct Running(Arc<AtomicBool>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Wraps `emit` so the controller learns whether a compaction completed
/// during the operation.
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

impl Controller {
    /// Takes the lines background reflection has queued since the last call.
    pub fn take_notices(&self) -> Vec<String> {
        self.notices.take()
    }

    /// Queues a notice as background reflection would, for frontend tests.
    #[cfg(test)]
    pub(crate) fn push_notice(&self, line: &str) {
        self.notices.push(line.to_owned());
    }

    /// A signal that changes whenever [`Controller::take_notices`] has
    /// something new, for frontends that wait on events.
    pub fn notices_changed(&self) -> watch::Receiver<u64> {
        self.notices.signal.subscribe()
    }

    /// Starts a background reflection run when `[reflection].auto` is
    /// `on_compaction` and nothing else is running. Never blocks and never
    /// fails the operation that triggered it.
    pub(super) fn start_auto_reflection(&self) {
        let reflector = Arc::clone(&self.builder.reflector);
        if reflector.auto() != Auto::OnCompaction || !self.dynamic_content {
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
        tokio::spawn(async move {
            let _running = running;
            let context = reflection::Context {
                runner: &runner,
                session_id: &session_id,
                session_path: &session_path,
                service: &service,
                user_scope: &user_scope,
                workspace_scope: &workspace_scope,
                skill_roots: skill_roots.as_ref(),
            };
            let result = reflector
                .run(&context, Trigger::OnCompaction, "", &cancel)
                .await;
            if let Some(line) = notice_for(result) {
                notices.push(line);
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
        notice_for(self.reflect_as(Trigger::OnExit, "", cancel).await)
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
        let context = reflection::Context {
            runner: &runner,
            session_id: &session_id,
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

/// The line an automatic run is worth: what it changed, or why it failed.
/// Nothing for a run that found nothing, was skipped, or was cancelled.
fn notice_for(result: Result<Report, Error>) -> Option<String> {
    match result {
        Ok(report) if report.changed_something() => Some(report.line()),
        Ok(_) | Err(Error::Cancelled | Error::Disabled | Error::NoSession) => None,
        Err(error) => Some(format!("reflection: {error}")),
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

    #[test]
    fn only_runs_that_changed_something_or_failed_are_announced() {
        let skipped = Report::skipped_for_test();
        assert_eq!(notice_for(Ok(skipped)), None);
        assert_eq!(notice_for(Err(Error::Cancelled)), None);
        assert_eq!(notice_for(Err(Error::Disabled)), None);
        let failure = notice_for(Err(Error::Model("boom".into()))).expect("a failure is shown");
        assert!(failure.starts_with("reflection: "), "{failure}");
    }

    // -- end to end: a compaction starts a background run ---------------------

    use std::sync::atomic::AtomicUsize;

    use otto_core::agent::summary::SUMMARIZATION_SYSTEM_PROMPT;
    use otto_core::model::{FinishReason, Message, Role};
    use otto_core::operation::OperationControl;
    use otto_core::provider::{Provider, ProviderSettlement, Request, Response, StreamSink};
    use otto_core::session::Session;

    use crate::cli::runtime_builder::Runner;
    use crate::cli::testutil::{builder, initial_runtime};
    use crate::subagent::tasks::Tasks;

    const SUMMARY: &str = "## Goal\nx\n## Constraints & Preferences\nx\n## Observations\nx\n## Progress\n### Done\nx\n### In Progress\nx\n### Blocked\nx\n## Key Decisions\nx\n## Next Steps\nx\n## Critical Context\nx";

    /// Answers a compaction request with a valid summary and a reflection
    /// request with `reflection`, counting each.
    struct Router {
        reflection: String,
        compactions: AtomicUsize,
        reflections: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for Router {
        async fn complete(
            &self,
            request: &Request,
            _emit: StreamSink<'_>,
            _control: &dyn OperationControl,
        ) -> ProviderSettlement {
            let text = if request
                .system_prompt
                .starts_with(SUMMARIZATION_SYSTEM_PROMPT)
            {
                self.compactions.fetch_add(1, Ordering::SeqCst);
                SUMMARY.to_owned()
            } else {
                self.reflections.fetch_add(1, Ordering::SeqCst);
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
        use crate::cli::wiring::{MemoryWiring, open_memory_service, workspace_memory_scope};
        use crate::reflection::{Reflector, Store};
        use otto_core::config::ReflectionRuntime;
        use otto_core::config::memory::MemoryRuntime;

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let memory = tempfile::tempdir().expect("memory");
        let runtime_config = MemoryRuntime {
            enabled: true,
            backend: "sqlite".into(),
            sqlite_path: memory
                .path()
                .join("memory.db")
                .to_string_lossy()
                .into_owned(),
            ..MemoryRuntime::default()
        };
        let (service, user_scope, usable) =
            open_memory_service(&runtime_config, &[], &mut Vec::new()).expect("memory");
        let mut builder = builder(workspace.path(), sessions.path());
        builder.shared_mut().home = home.path().to_string_lossy().into_owned();
        builder.shared_mut().memory = MemoryWiring {
            service: Arc::clone(&service),
            usable,
            user_scope: user_scope.clone(),
            recall_limit: 8,
            recall_token_budget: 1000,
        };
        builder.workspace_scope =
            workspace_memory_scope(&runtime_config, &workspace.path().to_string_lossy())
                .expect("workspace scope");
        builder.shared_mut().reflector = Arc::new(Reflector::new(
            ReflectionRuntime {
                auto,
                skills: false,
                ..ReflectionRuntime::default()
            },
            Some(Arc::new(Store::open_in_memory().expect("store"))),
        ));
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
        let reflection = serde_json::json!({"memories": [{
            "action": "create", "scope": "user", "kind": "preference", "key": "language",
            "text": "Answer in Chinese", "confidence": 0.9, "reason": "the user said so",
            "evidence": [{"entry": user_ids.first().cloned().unwrap_or_default(), "quote": "always answer in Chinese"}],
        }]})
        .to_string();
        let provider = Arc::new(Router {
            reflection,
            compactions: AtomicUsize::new(0),
            reflections: AtomicUsize::new(0),
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
            if let Some(line) = notices.into_iter().next() {
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

    #[tokio::test]
    async fn a_compaction_starts_one_background_reflection_and_queues_a_notice() {
        let rig = rig(Auto::OnCompaction, 6).await;
        compact(&rig).await;
        assert_eq!(
            rig.provider.compactions.load(Ordering::SeqCst),
            1,
            "the compaction ran"
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
}
