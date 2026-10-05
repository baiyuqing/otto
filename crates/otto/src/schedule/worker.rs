//! The loops that run scheduled prompts and deliver ready items.
//!
//! One [`Worker`] runs in each long-lived process. The run role and the
//! delivery role are separate loops joined in one task: a run takes minutes
//! and must not delay deliveries. A store error is written as
//! `schedule_store_error` to the worker's [`Logger`] (a discard logger in the
//! terminal frontends, which own the screen); the next iteration retries.
//!
//! Cancellation: when the token passed to [`Worker::run`] is cancelled, a run
//! in progress is cancelled and awaited and nothing is recorded, so the lease
//! expires and another process can claim the occurrence. A delivery in
//! progress is dropped the same way.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio_util::sync::CancellationToken;

use super::store::{Item, delivery_text, run_text};
use super::{Origin, RunOutcome, Scope, Store};
use crate::log::Logger;
use crate::server::metrics::Metrics;

/// A run is cancelled after this long; the claim lease is 15 min.
const RUN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DELIVERY_LEASE: Duration = Duration::from_secs(3 * 60);
const DELIVERY_BATCH: usize = 10;
/// Longest sleep when nothing is claimable.
const MAX_IDLE: Duration = Duration::from_secs(15);
/// Shortest sleep. `next_due` ignores scope, so a row this worker may not
/// handle can be claimable forever; the floor keeps that from spinning.
const MIN_IDLE: Duration = Duration::from_secs(1);

#[async_trait::async_trait]
pub trait PromptRunner: Send + Sync {
    /// Runs `text` as the first user message of a new session in `workspace`.
    /// Maps its own cancellation to an outcome; the worker decides whether to
    /// record it.
    async fn run(&self, workspace: &str, text: &str, cancel: &CancellationToken) -> RunOutcome;
}

#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    /// Delivers `text` to `origin`. `Err` is a failed attempt, retried later.
    async fn deliver(&self, origin: &Origin, text: &str) -> Result<(), String>;
}

pub struct RunRole {
    pub scope: Scope,
    /// Only items created in this workspace, when set.
    pub workspace: Option<String>,
    pub runner: Arc<dyn PromptRunner>,
}

pub struct DeliverRole {
    pub scope: Scope,
    pub sink: Arc<dyn Sink>,
}

pub struct Worker {
    pub store: Arc<Store>,
    pub run: Option<RunRole>,
    pub deliver: Option<DeliverRole>,
    pub logger: Arc<Logger>,
    /// The `/metrics` registry of this process; `None` where there is no
    /// `/metrics` endpoint.
    pub metrics: Option<Arc<Metrics>>,
}

/// What the loops write their events and counters to.
struct Telemetry {
    logger: Arc<Logger>,
    metrics: Option<Arc<Metrics>>,
}

impl Telemetry {
    fn store_error(&self, operation: &str, error: impl std::fmt::Display) {
        self.logger.warn(
            "schedule_store_error",
            &[
                ("operation", operation.to_string()),
                ("error", error.to_string()),
            ],
        );
    }
}

/// The stable `outcome` word of a run, in the log and the metric label.
fn outcome_label(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Reply(_) => "reply",
        RunOutcome::FailedBeforeTools(_) => "failed_before_tools",
        RunOutcome::Failed(_) => "failed",
    }
}

impl Worker {
    /// Runs until `cancel`. Each loop: sweep, claim and process until nothing
    /// is claimable, then wait until `min(next_due, now + 15 s)`, the store's
    /// notifier, or cancellation.
    pub async fn run(self, cancel: CancellationToken) {
        let Self {
            store,
            run,
            deliver,
            logger,
            metrics,
        } = self;
        let telemetry = Telemetry { logger, metrics };
        let runs = async {
            if let Some(role) = &run {
                run_loop(&store, role, &telemetry, &cancel).await;
            }
        };
        let deliveries = async {
            if let Some(role) = &deliver {
                deliver_loop(&store, role, &telemetry, &cancel).await;
            }
        };
        tokio::join!(runs, deliveries);
    }
}

async fn run_loop(
    store: &Store,
    role: &RunRole,
    telemetry: &Telemetry,
    cancel: &CancellationToken,
) {
    // ponytail: prompt runs execute one at a time per worker, so items due in
    // the same minute wait for the runs ahead of them (up to 10 min each).
    // Run them concurrently if that delay matters.
    idle_loop(store, cancel, |now| async move {
        if let Err(error) = store.sweep(now) {
            telemetry.store_error("sweep", error);
        }
        let item = match store.claim_run(&role.scope, role.workspace.as_deref(), now) {
            Ok(Some(item)) => item,
            Ok(None) => return false,
            Err(error) => {
                telemetry.store_error("claim_run", error);
                return false;
            }
        };
        run_one(store, role, telemetry, item, cancel).await;
        true
    })
    .await;
}

async fn run_one(
    store: &Store,
    role: &RunRole,
    telemetry: &Telemetry,
    item: Item,
    cancel: &CancellationToken,
) {
    let started = std::time::Instant::now();
    let text = run_text(&item, Utc::now());
    let run_cancel = cancel.child_token();
    let run = role.runner.run(&item.workspace, &text, &run_cancel);
    tokio::pin!(run);
    let timeout = tokio::time::sleep(RUN_TIMEOUT);
    tokio::pin!(timeout);
    let mut timed_out = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut run => break outcome,
            _ = &mut timeout, if !timed_out => {
                timed_out = true;
                run_cancel.cancel();
            }
        }
    };
    if cancel.is_cancelled() {
        return;
    }
    let outcome = if timed_out {
        RunOutcome::Failed("the run did not finish within 10 min".into())
    } else {
        outcome
    };
    let label = outcome_label(&outcome);
    if let Err(error) = store.finish_run(item.id, item.due_at, outcome, Utc::now()) {
        telemetry.store_error("finish_run", error);
        return;
    }
    telemetry.logger.info(
        "schedule_run_finished",
        &[
            ("schedule_id", item.id.to_string()),
            (
                "due_at",
                item.due_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            ("outcome", label.to_string()),
            ("duration_ms", started.elapsed().as_millis().to_string()),
        ],
    );
    if let Some(metrics) = &telemetry.metrics {
        metrics.schedule_run(label);
    }
}

async fn deliver_loop(
    store: &Store,
    role: &DeliverRole,
    telemetry: &Telemetry,
    cancel: &CancellationToken,
) {
    idle_loop(store, cancel, |now| async move {
        if let Err(error) = store.sweep(now) {
            telemetry.store_error("sweep", error);
        }
        let items = match store.claim_deliveries(&role.scope, DELIVERY_LEASE, DELIVERY_BATCH, now) {
            Ok(items) => items,
            Err(error) => {
                telemetry.store_error("claim_deliveries", error);
                return false;
            }
        };
        let progressed = !items.is_empty();
        for item in items {
            let text = delivery_text(&item, now);
            let sent = tokio::select! {
                sent = role.sink.deliver(&item.origin, &text) => sent,
                _ = cancel.cancelled() => return progressed,
            };
            // The platform, or `terminal`/`web`: never the chat id, which grows
            // without bound.
            let channel = item.origin.chat_platform().unwrap_or(item.origin.as_str());
            let (outcome, recorded) = match &sent {
                Ok(()) => {
                    telemetry.logger.info(
                        "schedule_delivery_finished",
                        &[("channel", channel.to_string()), ("outcome", "ok".into())],
                    );
                    (
                        "ok",
                        store
                            .delivered(item.id, item.due_at, Utc::now())
                            .map(|_| ())
                            .map_err(|error| ("delivered", error)),
                    )
                }
                Err(error) => {
                    telemetry.logger.warn(
                        "schedule_delivery_finished",
                        &[
                            ("channel", channel.to_string()),
                            ("outcome", "error".into()),
                            ("error", error.clone()),
                        ],
                    );
                    (
                        "error",
                        store
                            .delivery_failed(item.id, item.due_at, error, Utc::now())
                            .map_err(|error| ("delivery_failed", error)),
                    )
                }
            };
            if let Some(metrics) = &telemetry.metrics {
                metrics.schedule_delivery(channel, outcome);
            }
            if let Err((operation, error)) = recorded {
                telemetry.store_error(operation, error);
            }
        }
        progressed
    })
    .await;
}

/// Calls `step` until it reports no progress, then sleeps. The notifier wait
/// is armed before `step` so a wake-up during the step is not lost.
async fn idle_loop<F, Fut>(store: &Store, cancel: &CancellationToken, mut step: F)
where
    F: FnMut(DateTime<Utc>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    loop {
        let notified = store.notifier().notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if cancel.is_cancelled() {
            return;
        }
        if step(Utc::now()).await {
            continue;
        }
        let now = Utc::now();
        let wait = match store.next_due(now) {
            Ok(Some(at)) => (at - now).clamp(TimeDelta::zero(), TimeDelta::MAX),
            _ => TimeDelta::MAX,
        }
        .to_std()
        .unwrap_or(MAX_IDLE)
        .clamp(MIN_IDLE, MAX_IDLE);
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = &mut notified => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::schedule::{Kind, NewItem, State};

    fn telegram() -> Origin {
        Origin::chat("telegram", "42").unwrap()
    }

    fn new_item(origin: Origin, kind: Kind, body: &str) -> NewItem {
        NewItem {
            origin,
            session_id: "sess".into(),
            workspace: "/work".into(),
            kind,
            body: body.into(),
            repeat: None,
            local_time: None,
            due_at: Utc::now(),
        }
    }

    #[derive(Default)]
    struct FakeSink {
        sent: Mutex<Vec<(Origin, String)>>,
        fail_with: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl Sink for FakeSink {
        async fn deliver(&self, origin: &Origin, text: &str) -> Result<(), String> {
            if let Some(error) = self.fail_with.lock().unwrap().clone() {
                return Err(error);
            }
            self.sent
                .lock()
                .unwrap()
                .push((origin.clone(), text.to_string()));
            Ok(())
        }
    }

    impl FakeSink {
        fn sent(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(_, text)| text.clone())
                .collect()
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        outcomes: Mutex<VecDeque<RunOutcome>>,
        calls: Mutex<Vec<(String, String)>>,
        /// Wait for cancellation instead of returning.
        block: bool,
    }

    #[async_trait::async_trait]
    impl PromptRunner for FakeRunner {
        async fn run(&self, workspace: &str, text: &str, cancel: &CancellationToken) -> RunOutcome {
            self.calls
                .lock()
                .unwrap()
                .push((workspace.into(), text.into()));
            if self.block {
                cancel.cancelled().await;
                return RunOutcome::Reply("late reply after cancellation".into());
            }
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("an outcome")
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn logger(&self) -> Arc<Logger> {
            Arc::new(Logger::new(Box::new(self.clone())))
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    struct Harness {
        log: Capture,
        metrics: Arc<Metrics>,
        store: Arc<Store>,
        sink: Arc<FakeSink>,
        runner: Arc<FakeRunner>,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    fn start(runner: FakeRunner, deliver_scope: Scope, run_workspace: Option<&str>) -> Harness {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let sink = Arc::new(FakeSink::default());
        let runner = Arc::new(runner);
        let cancel = CancellationToken::new();
        let log = Capture::default();
        let metrics = Arc::new(Metrics::new());
        let worker = Worker {
            store: Arc::clone(&store),
            run: Some(RunRole {
                scope: Scope::Any,
                workspace: run_workspace.map(str::to_owned),
                runner: runner.clone(),
            }),
            deliver: Some(DeliverRole {
                scope: deliver_scope,
                sink: sink.clone(),
            }),
            logger: log.logger(),
            metrics: Some(Arc::clone(&metrics)),
        };
        let task = tokio::spawn(worker.run(cancel.clone()));
        Harness {
            log,
            metrics,
            store,
            sink,
            runner,
            cancel,
            task,
        }
    }

    impl Harness {
        fn create(&self, item: NewItem) -> i64 {
            self.store.create(item, Utc::now()).unwrap().id
        }

        fn item(&self, origin: &Origin, id: i64) -> crate::schedule::Item {
            self.store
                .list(origin, Utc::now())
                .unwrap()
                .into_iter()
                .find(|item| item.id == id)
                .unwrap()
        }

        async fn stop(self) {
            self.cancel.cancel();
            self.task.await.unwrap();
        }
    }

    async fn wait_for(step: Duration, mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(3600), async {
            while !condition() {
                tokio::time::sleep(step).await;
            }
        })
        .await
        .expect("condition not reached");
    }

    const STEP: Duration = Duration::from_millis(5);

    #[tokio::test]
    async fn a_due_text_item_is_delivered_once() {
        let harness = start(FakeRunner::default(), Scope::Any, None);
        let id = harness.create(new_item(telegram(), Kind::Text, "stand up"));
        wait_for(STEP, || harness.item(&telegram(), id).state == State::Done).await;
        assert_eq!(harness.sink.sent(), ["⏰ stand up"]);
        assert_eq!(harness.sink.sent.lock().unwrap()[0].0, telegram());
        harness.stop().await;
    }

    #[tokio::test]
    async fn a_finished_run_writes_its_event_and_counts_it() {
        let runner = FakeRunner::default();
        runner
            .outcomes
            .lock()
            .unwrap()
            .push_back(RunOutcome::Reply("done".into()));
        let harness = start(runner, Scope::Any, Some("/work"));
        let id = harness.create(new_item(telegram(), Kind::Prompt, "p"));
        wait_for(STEP, || harness.item(&telegram(), id).state == State::Done).await;
        let text = harness.log.text();
        let line = text
            .lines()
            .find(|line| line.contains("msg=schedule_run_finished"))
            .unwrap_or_else(|| panic!("no run event in:\n{text}"));
        for field in [
            "level=INFO",
            &format!("schedule_id={id}"),
            "outcome=reply",
            "duration_ms=",
            "due_at=",
        ] {
            assert!(line.contains(field), "{field} missing in {line}");
        }
        assert!(
            harness
                .metrics
                .render()
                .contains("otto_schedule_runs_total{outcome=\"reply\"} 1")
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn a_failed_delivery_writes_a_warning_and_counts_it() {
        let harness = start(FakeRunner::default(), Scope::Any, None);
        *harness.sink.fail_with.lock().unwrap() = Some("platform down".into());
        // A distinctive opaque ID cannot accidentally match the log timestamp.
        let chat_id = "private-chat-id-42";
        let origin = Origin::chat("telegram", chat_id).unwrap();
        let id = harness.create(new_item(origin.clone(), Kind::Text, "hi"));
        wait_for(STEP, || harness.item(&origin, id).attempts == 1).await;
        let text = harness.log.text();
        let line = text
            .lines()
            .find(|line| line.contains("msg=schedule_delivery_finished"))
            .unwrap_or_else(|| panic!("no delivery event in:\n{text}"));
        for field in [
            "level=WARN",
            "channel=telegram",
            "outcome=error",
            "error=\"platform down\"",
        ] {
            assert!(line.contains(field), "{field} missing in {line}");
        }
        assert!(
            !line.contains(chat_id),
            "the chat id must not be logged: {line}"
        );
        assert!(
            harness.metrics.render().contains(
                "otto_schedule_deliveries_total{channel=\"telegram\",outcome=\"error\"} 1"
            )
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn a_store_failure_is_written_as_a_warning() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.poison_for_test();
        let log = Capture::default();
        let cancel = CancellationToken::new();
        let worker = Worker {
            store,
            run: Some(RunRole {
                scope: Scope::Any,
                workspace: None,
                runner: Arc::new(FakeRunner::default()),
            }),
            deliver: None,
            logger: log.logger(),
            metrics: None,
        };
        let task = tokio::spawn(worker.run(cancel.clone()));
        wait_for(STEP, || log.text().contains("operation=claim_run")).await;
        cancel.cancel();
        task.await.unwrap();
        let text = log.text();
        assert!(
            text.contains("level=WARN msg=schedule_store_error operation=sweep"),
            "{text}"
        );
        assert!(
            text.contains("error=\"schedule store unavailable\""),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_failed_send_is_recorded_for_a_later_retry() {
        let harness = start(FakeRunner::default(), Scope::Any, None);
        *harness.sink.fail_with.lock().unwrap() = Some("platform down".into());
        let id = harness.create(new_item(telegram(), Kind::Text, "hi"));
        wait_for(STEP, || harness.item(&telegram(), id).attempts == 1).await;
        let row = harness.item(&telegram(), id);
        assert_eq!(
            (row.state, row.last_error.as_str()),
            (State::Ready, "platform down")
        );
        assert!(row.next_try_at > Utc::now() + chrono::TimeDelta::seconds(20));
        assert!(harness.sink.sent().is_empty());
        harness.stop().await;
    }

    #[tokio::test]
    async fn a_prompt_reply_is_delivered_after_a_run_in_the_item_workspace() {
        let runner = FakeRunner::default();
        runner
            .outcomes
            .lock()
            .unwrap()
            .push_back(RunOutcome::Reply("sunny, 24C".into()));
        let harness = start(runner, Scope::Any, Some("/work"));
        let id = harness.create(new_item(telegram(), Kind::Prompt, "weather in Beijing"));
        wait_for(STEP, || harness.item(&telegram(), id).state == State::Done).await;
        assert_eq!(harness.sink.sent(), ["⏰ sunny, 24C"]);
        let calls = harness.runner.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/work");
        assert!(
            calls[0].1.ends_with("\n\nweather in Beijing"),
            "{}",
            calls[0].1
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn a_failure_before_tools_is_retried_not_delivered() {
        let runner = FakeRunner::default();
        runner
            .outcomes
            .lock()
            .unwrap()
            .push_back(RunOutcome::FailedBeforeTools("503".into()));
        let harness = start(runner, Scope::Any, None);
        let id = harness.create(new_item(telegram(), Kind::Prompt, "p"));
        wait_for(STEP, || harness.item(&telegram(), id).attempts == 1).await;
        let row = harness.item(&telegram(), id);
        assert_eq!(
            (row.state, row.last_error.as_str()),
            (State::Waiting, "503")
        );
        assert!(harness.sink.sent().is_empty());
        harness.stop().await;
    }

    #[tokio::test]
    async fn a_failure_after_tools_is_delivered_as_failure_text() {
        let runner = FakeRunner::default();
        runner
            .outcomes
            .lock()
            .unwrap()
            .push_back(RunOutcome::Failed("tool crashed".into()));
        let harness = start(runner, Scope::Any, None);
        let id = harness.create(new_item(telegram(), Kind::Prompt, "p"));
        wait_for(STEP, || harness.item(&telegram(), id).state == State::Done).await;
        assert_eq!(
            harness.sink.sent(),
            [format!("⏰ Scheduled prompt s{id} failed: tool crashed")]
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn items_outside_the_scope_or_workspace_are_left_alone() {
        let harness = start(FakeRunner::default(), Scope::Web, Some("/elsewhere"));
        let text = harness.create(new_item(telegram(), Kind::Text, "chat text"));
        let prompt = harness.create(new_item(Origin::web(), Kind::Prompt, "p"));
        let web = harness.create(new_item(Origin::web(), Kind::Text, "web text"));
        wait_for(STEP, || {
            harness.item(&Origin::web(), web).state == State::Done
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(harness.sink.sent(), ["⏰ web text"]);
        assert_eq!(harness.item(&telegram(), text).state, State::Waiting);
        assert_eq!(harness.item(&Origin::web(), prompt).state, State::Waiting);
        assert!(harness.runner.calls.lock().unwrap().is_empty());
        harness.stop().await;
    }

    #[tokio::test]
    async fn cancelling_the_worker_cancels_the_run_and_records_nothing() {
        let harness = start(
            FakeRunner {
                block: true,
                ..FakeRunner::default()
            },
            Scope::Any,
            None,
        );
        let id = harness.create(new_item(telegram(), Kind::Prompt, "p"));
        wait_for(STEP, || !harness.runner.calls.lock().unwrap().is_empty()).await;
        let store = Arc::clone(&harness.store);
        harness.stop().await;
        let row = store.list(&telegram(), Utc::now()).unwrap().remove(0);
        assert_eq!((row.id, row.state, row.attempts), (id, State::Waiting, 0));
        assert!(
            row.claimed_until > Utc::now(),
            "the lease stays until it expires"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_is_cancelled_after_ten_minutes_and_recorded_as_failed() {
        let harness = start(
            FakeRunner {
                block: true,
                ..FakeRunner::default()
            },
            Scope::Any,
            None,
        );
        let id = harness.create(new_item(telegram(), Kind::Prompt, "p"));
        wait_for(Duration::from_secs(1), || {
            harness.item(&telegram(), id).state == State::Done
        })
        .await;
        assert_eq!(
            harness.sink.sent(),
            [format!(
                "⏰ Scheduled prompt s{id} failed: the run did not finish within 10 min"
            )]
        );
        harness.stop().await;
    }
}
