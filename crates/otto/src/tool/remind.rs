//! The `remind` tool: schedule a wake without blocking the turn.
//!
//! A call returns as soon as the timer is registered. When it fires, a
//! `[timer]` notification is pushed into the session inbox so the existing
//! wake loop (REPL, TUI, `otto serve`) starts an empty-text turn. There is
//! no second scheduler. File-backed sessions keep outstanding timers beside
//! the JSONL (`{id}.reminders.json`); opening that session restores them.
//! Dropping the tool cancels in-process sleeps but leaves the file, so a
//! later resume can fire. `/new` starts a different session id and does not
//! inherit. `--no-session` has nothing to write and stays process-local.
//!
//! Ownership: the tool holds the parent's inbox, an optional persist file,
//! and a session-scoped cancel token. Concurrency: `execute` takes `&self`
//! and may run concurrently; inflight timers are counted atomically.
//! Cancellation: the *turn* token only aborts a call that has not yet
//! spawned; the timer itself lives on the session token so a finished turn
//! cannot kill it.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use otto_core::agent::inbox::{Inbox, Notification};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::decode_strict_json;
use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};

const MIN_SECONDS: i64 = 1;
const MAX_SECONDS: i64 = 3600;
const MAX_MESSAGE_CHARS: usize = 500;
const MAX_INFLIGHT: usize = 8;
const PERSIST_MUTEX: &str = "reminder persist mutex";

const DESCRIPTION: &str = "Schedule a reminder that arrives later as a [timer] message and starts a wake turn. Returns immediately; does not block. Timers are kept with the session across a restart (at most 8 at once, 1 to 3600 seconds); /new starts a fresh session without them. Use this when you need to continue after a delay without waiting in the current turn.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemindArgs {
    seconds: i64,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredReminder {
    id: String,
    fire_at: DateTime<Utc>,
    message: String,
}

struct Persist {
    path: PathBuf,
    items: Vec<StoredReminder>,
}

impl Persist {
    fn load(path: PathBuf) -> Self {
        let items = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path, items }
    }

    fn insert(&mut self, item: StoredReminder) -> Result<(), String> {
        if self.items.len() >= MAX_INFLIGHT {
            return Err(format!("too many reminders (max {MAX_INFLIGHT})"));
        }
        self.items.push(item);
        if let Err(error) = self.save() {
            self.items.pop();
            return Err(error);
        }
        Ok(())
    }

    fn forget(&mut self, id: &str) {
        self.items.retain(|stored| stored.id != id);
        let _ = self.save();
    }

    fn save(&self) -> Result<(), String> {
        if self.items.is_empty() {
            if let Err(error) = std::fs::remove_file(&self.path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("remove reminders: {error}"));
            }
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create reminder directory: {error}"))?;
        }
        let mut tmp = self.path.clone();
        tmp.as_mut_os_string().push(".tmp");
        let body = serde_json::to_vec_pretty(&self.items)
            .map_err(|error| format!("encode reminders: {error}"))?;
        write_private(&tmp, &body)?;
        std::fs::rename(&tmp, &self.path).map_err(|error| format!("persist reminders: {error}"))
    }
}

fn write_private(path: &Path, body: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("write reminders: {error}"))?;
    file.write_all(body)
        .map_err(|error| format!("write reminders: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync reminders: {error}"))
}

/// Schedules timer notifications into `inbox`, optionally persisting them.
pub struct RemindTool {
    inbox: Arc<Inbox>,
    cancel: CancellationToken,
    inflight: Arc<AtomicUsize>,
    persist: Option<Arc<Mutex<Persist>>>,
    next_id: AtomicUsize,
}

impl RemindTool {
    pub fn new(inbox: Arc<Inbox>) -> Self {
        Self::create(inbox, None)
    }

    /// Restores any reminders already stored at `path`, then keeps writing
    /// new ones there. A due reminder fires as soon as the runtime polls it.
    pub fn with_persist(inbox: Arc<Inbox>, path: PathBuf) -> Self {
        let persist = Persist::load(path);
        let pending = persist.items.clone();
        let tool = Self::create(inbox, Some(Arc::new(Mutex::new(persist))));
        for item in pending.into_iter().take(MAX_INFLIGHT) {
            let _ = tool.arm(item, false);
        }
        tool
    }

    fn create(inbox: Arc<Inbox>, persist: Option<Arc<Mutex<Persist>>>) -> Self {
        Self {
            inbox,
            cancel: CancellationToken::new(),
            inflight: Arc::new(AtomicUsize::new(0)),
            persist,
            next_id: AtomicUsize::new(0),
        }
    }

    fn schedule(&self, delay: Duration, message: String) -> Result<(), String> {
        let id = format!("r{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let fire_at = Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default();
        self.arm(
            StoredReminder {
                id,
                fire_at,
                message,
            },
            true,
        )
    }

    fn arm(&self, item: StoredReminder, write: bool) -> Result<(), String> {
        if self.inflight.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            return Err(format!("too many reminders (max {MAX_INFLIGHT})"));
        }
        if write
            && let Some(store) = &self.persist
            && let Err(error) = store.lock().expect(PERSIST_MUTEX).insert(item.clone())
        {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            return Err(error);
        }
        let delay = delay_until(item.fire_at);
        let inbox = Arc::clone(&self.inbox);
        let cancel = self.cancel.clone();
        let inflight = Arc::clone(&self.inflight);
        let persist = self.persist.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(delay) => {
                    inbox.push(Notification {
                        task_id: "timer".into(),
                        text: format!("[timer] {}", item.message),
                        ..Notification::default()
                    });
                    if let Some(persist) = persist {
                        persist.lock().expect(PERSIST_MUTEX).forget(&item.id);
                    }
                }
                () = cancel.cancelled() => {}
            }
            inflight.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(())
    }
}

fn delay_until(fire_at: DateTime<Utc>) -> Duration {
    (fire_at - Utc::now()).to_std().unwrap_or(Duration::ZERO)
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

    fn persist_setup() -> (tempfile::TempDir, PathBuf, Arc<Inbox>) {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("session.reminders.json");
        (directory, path, Arc::new(Inbox::default()))
    }

    async fn wait_until_fired(inbox: &Inbox) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while inbox.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the timer did not fire");
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
        wait_until_fired(&inbox).await;

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

    #[tokio::test]
    async fn a_schedule_is_written_beside_the_session() {
        let (_directory, path, inbox) = persist_setup();
        let tool = RemindTool::with_persist(Arc::clone(&inbox), path.clone());
        let result = run(&tool, r#"{"seconds":60,"message":"later"}"#).await;
        assert!(!result.is_error, "{result:?}");
        let raw = std::fs::read_to_string(&path).expect("persist file");
        assert!(raw.contains("later"), "{raw}");
        assert!(inbox.is_empty());
    }

    #[tokio::test]
    async fn reopening_fires_a_due_reminder_and_clears_it() {
        let (_directory, path, inbox) = persist_setup();
        std::fs::write(
            &path,
            r#"[{"id":"r1","fire_at":"2020-01-01T00:00:00Z","message":"due"}]"#,
        )
        .expect("write");
        let _tool = RemindTool::with_persist(Arc::clone(&inbox), path.clone());
        wait_until_fired(&inbox).await;
        let items = inbox.drain();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].text, "[timer] due");
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!raw.contains("due"), "{raw}");
    }

    #[tokio::test]
    async fn dropping_the_tool_keeps_unfired_reminders_on_disk() {
        let (_directory, path, inbox) = persist_setup();
        {
            let tool = RemindTool::with_persist(Arc::clone(&inbox), path.clone());
            let result = run(&tool, r#"{"seconds":60,"message":"keep"}"#).await;
            assert!(!result.is_error, "{result:?}");
        }
        let raw = std::fs::read_to_string(&path).expect("persist file");
        assert!(raw.contains("keep"), "{raw}");
    }
}
