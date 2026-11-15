//! The loops that run scheduled prompts and deliver ready items.
//!
//! A [`Worker`] runs per process role or ACP connection. The run role and the
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

use super::store::{Item, WorkspaceAdmit, delivery_text, run_text};
use super::{RunOutcome, ScheduleStore, Scope};
use crate::log::Logger;
use crate::metrics::Metrics;
use crate::session::origin::Origin;

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
    async fn run_item(&self, item: &Item, text: &str, cancel: &CancellationToken) -> RunOutcome {
        self.run(&item.workspace, text, cancel).await
    }
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
    /// Only items whose workspace this accepts are claimed; `None` accepts
    /// all. A rejected item gets no lease, so another process can claim it.
    pub admit: Option<Arc<WorkspaceAdmit>>,
    pub runner: Arc<dyn PromptRunner>,
}

pub struct DeliverRole {
    pub scope: Scope,
    pub sink: Arc<dyn Sink>,
}

pub struct Worker {
    pub store: Arc<dyn ScheduleStore>,
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
                run_loop(store.as_ref(), role, &telemetry, &cancel).await;
            }
        };
        let deliveries = async {
            if let Some(role) = &deliver {
                deliver_loop(store.as_ref(), role, &telemetry, &cancel).await;
            }
        };
        let heartbeats = async {
            if let Some(role) = &deliver
                && let Scope::DeliveryClient(id) = &role.scope
            {
                let mut ticker = tokio::time::interval(Duration::from_secs(15));
                let mut previous = Vec::new();
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        _ = ticker.tick() => match store.refresh_delivery_client(id, Utc::now()) {
                            Ok(conflicts) => {
                                if conflicts != previous {
                                    if !conflicts.is_empty() {
                                        telemetry.logger.warn(
                                            "schedule_delivery_conflict",
                                            &[
                                                ("platforms", conflicts.join(",")),
                                                (
                                                    "reason",
                                                    "multiple clients offer this platform; legacy delivery suspended".into(),
                                                ),
                                            ],
                                        );
                                    }
                                    previous = conflicts;
                                }
                            }
                            Err(error) => telemetry.store_error("refresh_delivery_client", error),
                        }
                    }
                }
            }
        };
        tokio::join!(runs, deliveries, heartbeats);
        if let Some(role) = &deliver
            && let Scope::DeliveryClient(id) = &role.scope
            && let Err(error) = store.unregister_delivery_client(id)
        {
            telemetry.store_error("unregister_delivery_client", error);
        }
    }
}

async fn run_loop(
    store: &dyn ScheduleStore,
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
        let item = match store.claim_run_admitting(
            &role.scope,
            role.workspace.as_deref(),
            role.admit.as_deref(),
            now,
        ) {
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
    store: &dyn ScheduleStore,
    role: &RunRole,
    telemetry: &Telemetry,
    item: Item,
    cancel: &CancellationToken,
) {
    let started = std::time::Instant::now();
    let text = run_text(&item, Utc::now());
    let run_cancel = cancel.child_token();
    let run = role.runner.run_item(&item, &text, &run_cancel);
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
    store: &dyn ScheduleStore,
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
async fn idle_loop<F, Fut>(store: &dyn ScheduleStore, cancel: &CancellationToken, mut step: F)
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
            // Overdue but unclaimable rows belong to another scope or an
            // ambiguous legacy destination; they must not drive a busy loop.
            Ok(Some(at)) if at > now => (at - now).clamp(TimeDelta::zero(), TimeDelta::MAX),
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
mod tests;
