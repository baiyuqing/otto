//! One session's most recent turn, buffered away from the HTTP readers.
//!
//! Port of `internal/server/turn.go`. The agent calls [`Turn::emitter`]'s
//! closure synchronously and it performs no I/O, so a slow SSE reader never
//! applies backpressure to the provider loop. Readers take a
//! [`Turn::snapshot`] and wait on the [`tokio::sync::watch`] version for
//! more, which cannot miss an event: the append and the version bump happen
//! under the same lock, and a receiver created before a snapshot reports any
//! bump that follows it.
//!
//! ponytail: only the latest turn per session is retained, matching Go; add
//! a ring buffer of turns if replay across turns is ever needed.

use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, Utc};
use otto_core::agent::events::Event;
use otto_core::model::Usage;
use otto_core::wire::events::{WireEvent, to_wire};
use serde::Serialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::metrics::Metrics;

pub const TURN_RUNNING: &str = "running";
pub const TURN_OK: &str = "ok";
pub const TURN_ERROR: &str = "error";
pub const TURN_CANCELED: &str = "canceled";

/// Why a turn started. Port of Go's `triggerUser`/`triggerTask`.
pub const TRIGGER_USER: &str = "user";
pub const TRIGGER_TASK: &str = "task";

#[derive(Debug)]
struct TurnState {
    events: Vec<WireEvent>,
    done: bool,
    status: &'static str,
    error_text: String,
    text: String,
    usage: Usage,
    usage_present: bool,
    tool_start: Option<Instant>,
    started_at: DateTime<Utc>,
    started_instant: Instant,
    finished_at: Option<DateTime<Utc>>,
    elapsed: std::time::Duration,
}

/// The buffered turn.
#[derive(Debug)]
pub struct Turn {
    pub id: String,
    /// Set once, before the turn is published; read-only afterwards.
    pub trigger: &'static str,
    cancel: CancellationToken,
    state: Mutex<TurnState>,
    changed: watch::Sender<u64>,
}

impl Turn {
    pub fn new(id: String, trigger: &'static str, cancel: CancellationToken) -> Self {
        Self {
            id,
            trigger,
            cancel,
            state: Mutex::new(TurnState {
                events: Vec::new(),
                done: false,
                status: TURN_RUNNING,
                error_text: String::new(),
                text: String::new(),
                usage: Usage::default(),
                usage_present: false,
                tool_start: None,
                started_at: Utc::now(),
                started_instant: Instant::now(),
                finished_at: None,
                elapsed: std::time::Duration::ZERO,
            }),
            changed: watch::channel(0).0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn is_done(&self) -> bool {
        self.lock().done
    }

    /// `finished_at - started_at`; only meaningful once [`Turn::is_done`].
    pub fn elapsed(&self) -> std::time::Duration {
        self.lock().elapsed
    }

    /// A version receiver. Create it before reading a snapshot: every append
    /// bumps the version, so a bump that lands after the snapshot still wakes
    /// the reader.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Every event at index `after` onward, plus whether the turn finished.
    pub fn snapshot(&self, after: usize) -> (Vec<WireEvent>, bool) {
        let state = self.lock();
        let events = if after < state.events.len() {
            state.events[after..].to_vec()
        } else {
            Vec::new()
        };
        (events, state.done)
    }

    /// Records the terminal status. `error` is what the controller returned:
    /// `None` is ok, a cancellation is canceled, anything else is an error.
    pub fn finish(&self, error: Option<String>, canceled: bool) {
        {
            let mut state = self.lock();
            state.done = true;
            let finished = Utc::now();
            state.finished_at = Some(finished);
            state.elapsed = state.started_instant.elapsed();
            match error {
                None => state.status = TURN_OK,
                Some(_) if canceled => state.status = TURN_CANCELED,
                Some(message) => {
                    state.status = TURN_ERROR;
                    state.error_text = message;
                }
            }
        }
        self.changed.send_modify(|version| *version += 1);
    }

    pub fn summary(&self) -> TurnSummary {
        let state = self.lock();
        TurnSummary {
            id: self.id.clone(),
            trigger: self.trigger.to_string(),
            status: state.status.to_string(),
            error: state.error_text.clone(),
            text: state.text.clone(),
            usage: state.usage,
            usage_present: state.usage_present,
            started_at: state.started_at,
            finished_at: state.finished_at,
        }
    }

    /// The callback `Controller::prompt` and `WakeOperation::run` are given.
    /// It converts each event to wire form, accumulates text and usage,
    /// records metrics, and buffers the event for streaming readers.
    pub fn emitter<'a>(
        self: &'a std::sync::Arc<Self>,
        metrics: &'a std::sync::Arc<Metrics>,
    ) -> impl FnMut(Event) + Send + use<'a> {
        move |event: Event| {
            let mut wire = to_wire(&event);
            {
                let mut state = self.lock();
                match &event {
                    Event::AgentStarted => wire.turn_id = self.id.clone(),
                    Event::TextDelta { text } => state.text.push_str(text),
                    Event::ProviderUsage { usage, present } if *present => {
                        state.usage_present = true;
                        state.usage.input_tokens += usage.input_tokens;
                        state.usage.output_tokens += usage.output_tokens;
                        state.usage.cached_input_tokens += usage.cached_input_tokens;
                    }
                    Event::Notification { usage, present, .. } if *present => {
                        state.usage_present = true;
                        state.usage.input_tokens += usage.input_tokens;
                        state.usage.output_tokens += usage.output_tokens;
                        state.usage.cached_input_tokens += usage.cached_input_tokens;
                    }
                    Event::ProviderApiCall {
                        provider,
                        model,
                        duration,
                        status,
                    } => {
                        // Go records the metric and returns: the API call is
                        // not part of the event stream.
                        drop(state);
                        metrics.provider_api_request(provider, model, status.name(), *duration);
                        return;
                    }
                    Event::ToolCallStarted { .. } => state.tool_start = Some(Instant::now()),
                    Event::ToolCallFinished {
                        tool_name, result, ..
                    } => {
                        let elapsed = state
                            .tool_start
                            .map(|start| start.elapsed())
                            .unwrap_or_default();
                        let is_error = result.is_error;
                        let tool_name = tool_name.clone();
                        drop(state);
                        metrics.tool_call(&tool_name, is_error, elapsed);
                        let mut state = self.lock();
                        state.events.push(wire);
                        drop(state);
                        self.changed.send_modify(|version| *version += 1);
                        return;
                    }
                    _ => {}
                }
                state.events.push(wire);
            }
            self.changed.send_modify(|version| *version += 1);

            if let Event::ProviderUsage { usage, present } = &event
                && *present
            {
                metrics.tokens(usage);
            }
        }
    }
}

/// Port of Go's `turnSummary`. Field order matches, so the JSON bytes match.
#[derive(Debug, Clone, Serialize)]
pub struct TurnSummary {
    pub id: String,
    pub trigger: String,
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub text: String,
    pub usage: Usage,
    pub usage_present: bool,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::agent::events::ApiStatus;
    use otto_core::tool::ToolResult;
    use std::sync::Arc;

    fn turn() -> (Arc<Turn>, Arc<Metrics>) {
        (
            Arc::new(Turn::new(
                "t1".to_string(),
                TRIGGER_USER,
                CancellationToken::new(),
            )),
            Arc::new(Metrics::new()),
        )
    }

    fn usage(input: i64, output: i64, cached: i64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
        }
    }

    #[test]
    fn emit_accumulates_text_and_usage() {
        let (turn, metrics) = turn();
        let mut emit = turn.emitter(&metrics);
        emit(Event::AgentStarted);
        emit(Event::TextDelta {
            text: "hel".to_string(),
        });
        emit(Event::TextDelta {
            text: "lo".to_string(),
        });
        emit(Event::ProviderUsage {
            usage: usage(1, 2, 3),
            present: true,
        });
        emit(Event::ProviderUsage {
            usage: usage(10, 20, 0),
            present: true,
        });
        drop(emit);

        let (events, done) = turn.snapshot(0);
        assert!(!done);
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].turn_id, "t1");
        assert_eq!(events[1].turn_id, "");

        let summary = turn.summary();
        assert_eq!(summary.text, "hello");
        assert_eq!(summary.usage, usage(11, 22, 3));
        assert!(summary.usage_present);
        assert_eq!(summary.status, TURN_RUNNING);
        assert!(summary.finished_at.is_none());
        assert!(!turn.is_done());
    }

    #[test]
    fn emit_accumulates_notification_usage() {
        let (turn, metrics) = turn();
        let mut emit = turn.emitter(&metrics);
        emit(Event::ProviderUsage {
            usage: usage(1, 2, 3),
            present: true,
        });
        emit(Event::Notification {
            task_id: "t1".to_string(),
            text: "[task-notification] task t1 succeeded".to_string(),
            usage: usage(10, 20, 1),
            present: true,
        });
        drop(emit);
        assert_eq!(turn.summary().usage, usage(11, 22, 4));
    }

    #[test]
    fn usage_presence_distinguishes_missing_from_explicit_zero() {
        let (turn, metrics) = turn();
        let mut emit = turn.emitter(&metrics);
        emit(Event::ProviderUsage {
            usage: Usage::default(),
            present: false,
        });
        assert!(!turn.summary().usage_present);
        emit(Event::ProviderUsage {
            usage: Usage::default(),
            present: true,
        });
        drop(emit);
        let summary = turn.summary();
        assert!(summary.usage_present);
        assert_eq!(summary.usage, Usage::default());
    }

    #[test]
    fn a_provider_api_call_is_measured_but_never_buffered() {
        let (turn, metrics) = turn();
        let mut emit = turn.emitter(&metrics);
        emit(Event::ProviderApiCall {
            provider: "chatgpt".to_string(),
            model: "gpt-5".to_string(),
            duration: std::time::Duration::from_millis(1500),
            status: ApiStatus::Ok,
        });
        drop(emit);
        assert!(turn.snapshot(0).0.is_empty());
        assert!(metrics.render().contains(
            r#"otto_provider_api_requests_total{provider="chatgpt",model="gpt-5",status="ok"} 1"#
        ));
    }

    #[test]
    fn a_finished_tool_call_is_measured_and_buffered() {
        let (turn, metrics) = turn();
        let mut emit = turn.emitter(&metrics);
        emit(Event::ToolCallStarted {
            tool_name: "bash".to_string(),
            tool_call_id: "c1".to_string(),
            arguments: String::new(),
        });
        emit(Event::ToolCallFinished {
            tool_name: "bash".to_string(),
            tool_call_id: "c1".to_string(),
            result: ToolResult {
                content: "ok".to_string(),
                ..ToolResult::default()
            },
        });
        drop(emit);
        assert_eq!(turn.snapshot(0).0.len(), 2);
        assert!(
            metrics
                .render()
                .contains(r#"otto_tool_calls_total{tool="bash",status="ok"} 1"#)
        );
    }

    #[test]
    fn provider_tokens_are_counted_by_kind() {
        let (turn, metrics) = turn();
        let mut emit = turn.emitter(&metrics);
        emit(Event::ProviderUsage {
            usage: usage(5, 7, 0),
            present: true,
        });
        drop(emit);
        let body = metrics.render();
        assert!(body.contains(r#"otto_provider_tokens_total{kind="input"} 5"#));
        assert!(body.contains(r#"otto_provider_tokens_total{kind="output"} 7"#));
    }

    #[test]
    fn finish_sets_the_terminal_status() {
        for (error, canceled, want, want_error) in [
            (None, false, TURN_OK, false),
            (Some("canceled".to_string()), true, TURN_CANCELED, false),
            (Some("boom".to_string()), false, TURN_ERROR, true),
        ] {
            let (turn, _) = turn();
            turn.finish(error, canceled);
            assert!(turn.is_done());
            let summary = turn.summary();
            assert_eq!(summary.status, want);
            assert!(summary.finished_at.is_some());
            assert_eq!(!summary.error.is_empty(), want_error);
        }
    }

    #[test]
    fn a_snapshot_starts_at_an_arbitrary_sequence() {
        let (turn, metrics) = turn();
        let mut emit = turn.emitter(&metrics);
        for index in 0..5 {
            emit(Event::TextDelta {
                text: index.to_string(),
            });
        }
        drop(emit);
        let (events, done) = turn.snapshot(3);
        assert!(!done);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "3");
        assert_eq!(events[1].text, "4");
        assert!(turn.snapshot(5).0.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_readers_observe_every_event() {
        const COUNT: usize = 50;
        let (turn, metrics) = turn();

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let turn = Arc::clone(&turn);
                tokio::spawn(async move {
                    let mut changed = turn.subscribe();
                    let mut seen = Vec::new();
                    loop {
                        let (events, done) = turn.snapshot(seen.len());
                        seen.extend(events);
                        if done && seen.len() >= COUNT {
                            return seen.len();
                        }
                        if changed.changed().await.is_err() {
                            return seen.len();
                        }
                    }
                })
            })
            .collect();

        // Let every reader subscribe before the first event lands.
        tokio::task::yield_now().await;
        let mut emit = turn.emitter(&metrics);
        for index in 0..COUNT {
            emit(Event::TextDelta {
                text: index.to_string(),
            });
        }
        drop(emit);
        turn.finish(None, false);

        for reader in readers {
            assert_eq!(reader.await.expect("reader"), COUNT);
        }
    }
}
