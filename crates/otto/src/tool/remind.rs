//! The `remind` tool: schedule an in-process wake without blocking the turn.
//!
//! A call returns as soon as the timer is registered. When it fires, a
//! `[timer]` notification is pushed into the session inbox so the existing
//! wake loop (REPL, TUI, `otto serve`) starts an empty-text turn. There is
//! no second scheduler and nothing is persisted: replacing the session or
//! dropping the tool cancels outstanding timers, and exiting the process
//! forgets them.
//!
//! Ownership: the tool holds the parent's inbox and a session-scoped cancel
//! token. Concurrency: `execute` takes `&self` and may run concurrently;
//! inflight timers are counted atomically. Cancellation: the *turn* token
//! only aborts a call that has not yet spawned; the timer itself lives on
//! the session token so a finished turn cannot kill it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use otto_core::agent::inbox::{Inbox, Notification};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::decode_strict_json;
use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};

const MIN_SECONDS: i64 = 1;
const MAX_SECONDS: i64 = 3600;
const MAX_MESSAGE_CHARS: usize = 500;
const MAX_INFLIGHT: usize = 8;

const DESCRIPTION: &str = "Schedule a reminder that arrives later as a [timer] message and starts a wake turn. Returns immediately; does not block. Timers live only in this process (a restart or /new forgets them), at most 8 at once, 1 to 3600 seconds. Use this when you need to continue after a delay without waiting in the current turn.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemindArgs {
    seconds: i64,
    message: String,
}

/// Schedules in-process timer notifications into `inbox`.
pub struct RemindTool {
    inbox: Arc<Inbox>,
    cancel: CancellationToken,
    inflight: Arc<AtomicUsize>,
}

impl RemindTool {
    pub fn new(inbox: Arc<Inbox>) -> Self {
        Self {
            inbox,
            cancel: CancellationToken::new(),
            inflight: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn schedule(&self, delay: Duration, message: String) -> Result<(), String> {
        if self.inflight.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            return Err(format!("too many reminders (max {MAX_INFLIGHT})"));
        }
        let inbox = Arc::clone(&self.inbox);
        let cancel = self.cancel.clone();
        let inflight = Arc::clone(&self.inflight);
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(delay) => {
                    inbox.push(Notification {
                        task_id: "timer".into(),
                        text: format!("[timer] {message}"),
                        ..Notification::default()
                    });
                }
                () = cancel.cancelled() => {}
            }
            inflight.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(())
    }
}

impl Drop for RemindTool {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The schema advertised for `remind`.
pub fn remind_definition() -> ToolDefinition {
    definition(
        "remind",
        DESCRIPTION,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "seconds": {
                    "type": "integer",
                    "description": "Delay before the reminder fires, from 1 to 3600."
                },
                "message": {
                    "type": "string",
                    "description": "What to tell the next wake turn, at most 500 characters."
                }
            },
            "required": ["seconds", "message"]
        }),
    )
}

#[async_trait::async_trait]
impl Tool for RemindTool {
    fn definition(&self) -> ToolDefinition {
        remind_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: RemindArgs = match decode_strict_json(arguments.get(), &["seconds", "message"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let message = args.message.trim();
        if message.is_empty() {
            return error_result("message is required");
        }
        if message.chars().count() > MAX_MESSAGE_CHARS {
            return error_result(format!(
                "message is longer than {MAX_MESSAGE_CHARS} characters"
            ));
        }
        if args.seconds < MIN_SECONDS || args.seconds > MAX_SECONDS {
            return error_result(format!(
                "seconds must be between {MIN_SECONDS} and {MAX_SECONDS}"
            ));
        }
        if let Err(error) = self.schedule(
            Duration::from_secs(args.seconds as u64),
            message.to_string(),
        ) {
            return error_result(error);
        }
        text_result(format!("scheduled in {}s: {message}", args.seconds))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::tool::testutil::{run, run_cancelled};

    fn tool_and_inbox() -> (RemindTool, Arc<Inbox>) {
        let inbox = Arc::new(Inbox::default());
        (RemindTool::new(Arc::clone(&inbox)), inbox)
    }

    #[tokio::test]
    async fn a_call_returns_immediately_and_fires_after_the_delay() {
        let (tool, inbox) = tool_and_inbox();

        let result = run(&tool, r#"{"seconds":10,"message":"check the tests"}"#).await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "scheduled in 10s: check the tests");
        assert!(inbox.is_empty(), "the timer must not fire before the delay");

        tool.schedule(Duration::ZERO, "check the tests".into())
            .expect("zero-delay timer");
        tokio::time::timeout(Duration::from_secs(1), async {
            while inbox.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the timer did not fire");

        let items = inbox.drain();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].task_id, "timer");
        assert_eq!(items[0].text, "[timer] check the tests");
    }

    #[tokio::test]
    async fn dropping_the_tool_cancels_an_unfired_timer() {
        let inbox = Arc::new(Inbox::default());
        {
            let tool = RemindTool::new(Arc::clone(&inbox));
            tool.schedule(Duration::ZERO, "gone".into())
                .expect("schedule");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(inbox.is_empty(), "a dropped tool must not fire");
    }

    #[tokio::test]
    async fn invalid_arguments_are_rejected() {
        let (tool, inbox) = tool_and_inbox();
        for (arguments, snippet) in [
            (r#"{"seconds":0,"message":"x"}"#, "seconds must be between"),
            (
                r#"{"seconds":3601,"message":"x"}"#,
                "seconds must be between",
            ),
            (r#"{"seconds":1,"message":"   "}"#, "message is required"),
            (
                &format!(r#"{{"seconds":1,"message":"{}"}}"#, "x".repeat(501)),
                "message is longer than",
            ),
        ] {
            let result = run(&tool, arguments).await;
            assert!(result.is_error, "{arguments} => {result:?}");
            assert!(
                result.content.contains(snippet),
                "{arguments} => {}",
                result.content
            );
        }
        assert!(inbox.is_empty());
    }

    #[tokio::test]
    async fn more_than_eight_outstanding_reminders_are_rejected() {
        let (tool, _inbox) = tool_and_inbox();
        for index in 0..MAX_INFLIGHT {
            let arguments = format!(r#"{{"seconds":60,"message":"{index}"}}"#);
            let result = run(&tool, &arguments).await;
            assert!(!result.is_error, "{index}: {result:?}");
        }
        let overflow = run(&tool, r#"{"seconds":60,"message":"too many"}"#).await;
        assert!(overflow.is_error, "{overflow:?}");
        assert!(
            overflow.content.contains("too many reminders"),
            "{}",
            overflow.content
        );
    }

    #[tokio::test]
    async fn a_cancelled_turn_does_not_schedule() {
        let (tool, inbox) = tool_and_inbox();
        let result = run_cancelled(&tool, r#"{"seconds":1,"message":"nope"}"#).await;
        assert!(result.is_error, "{result:?}");
        assert_eq!(result.content, CONTEXT_CANCELED);
        assert!(inbox.is_empty());
    }
}
