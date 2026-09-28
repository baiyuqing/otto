//! The timer tools: `remind` schedules a wake without blocking the turn,
//! `remind_status` lists what is outstanding, `remind_cancel` stops one.
//!
//! A `remind` call returns as soon as the timer is registered. When it fires,
//! a `[timer]` notification is pushed into the session inbox so the existing
//! wake loop (REPL, TUI, `otto serve`) starts an empty-text turn. There is
//! no second scheduler. File-backed sessions keep outstanding timers beside
//! the JSONL (`{id}.reminders.json`); opening that session restores them.
//! Dropping the registry cancels in-process sleeps but leaves the file, so a
//! later resume can fire. `/new` starts a different session id and does not
//! inherit. Archiving ends a session for good: `session::archive` removes the
//! file and `app::Controller::archive_current_session` calls
//! [`Reminders::clear`]. `--no-session` has nothing to write and stays
//! process-local.
//!
//! Ownership: [`Reminders`] is the single record of what is armed and what is
//! on disk; the three tools and [`crate::cli::runtime_builder::Runner`] hold
//! `Arc`s of it, and `/timers` reads the runner's. Concurrency: `execute`
//! takes `&self` and may run concurrently, so every mutation goes through one
//! mutex. Cancellation: the *turn* token only aborts a call that has not yet
//! spawned; each timer sleeps on a child of the session token, so a finished
//! turn cannot kill it, [`Reminders::cancel`] stops one, and dropping the
//! registry stops all.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use otto_core::agent::inbox::{Inbox, Notification};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::subagent::format::round_to_seconds;

use super::result::decode_strict_json;
use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};

const MIN_SECONDS: i64 = 1;
const MAX_SECONDS: i64 = 3600;
const MAX_MESSAGE_CHARS: usize = 500;
const MAX_OUTSTANDING: usize = 8;
const STATE_MUTEX: &str = "reminder state mutex";
/// What `remind_status` and `/timers` print for an empty registry, in the
/// shape `agent_status` and `/tasks` use for theirs.
pub const NO_TIMERS: &str = "no timers in this session";

const DESCRIPTION: &str = "Schedule a reminder that arrives later as a [timer] message and starts a wake turn. Returns immediately; does not block. Timers are kept with the session across a restart (at most 8 at once, 1 to 3600 seconds); /new starts a fresh session without them, and archiving a session cancels them. Use this when you need to continue after a delay without waiting in the current turn. Use remind_status to see what is outstanding and remind_cancel to stop one.";

const STATUS_DESCRIPTION: &str = "List the timers outstanding in this session: one line each with the id, how long until it fires, and the message. Use the id with remind_cancel.";

const CANCEL_DESCRIPTION: &str = "Cancel one outstanding timer by id, as remind_status lists it. The timer never fires and is dropped from the session.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemindArgs {
    seconds: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemindCancelArgs {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemindStatusArgs {}

/// One outstanding timer, as it is stored and displayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredReminder {
    pub id: String,
    pub fire_at: DateTime<Utc>,
    pub message: String,
}

/// A stored timer plus the token that stops its sleep.
struct Armed {
    item: StoredReminder,
    token: CancellationToken,
}

/// The state the spawned timers share with the registry. Held by `Arc` so a
/// sleeping timer does not keep [`Reminders`] itself alive: dropping the
/// registry must still cancel.
struct Inner {
    inbox: Arc<Inbox>,
    /// `None` for `--no-session`, which has nothing to write.
    path: Option<PathBuf>,
    armed: Mutex<Vec<Armed>>,
    next_id: AtomicUsize,
}

impl Inner {
    /// Mirrors `armed` to disk. The caller holds the lock and passes the list
    /// so the file never disagrees with the registry.
    fn save(&self, armed: &[Armed]) -> Result<(), String> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if !session_file_exists(path, ".reminders.json") {
            // The session's own file is gone: archived, or (rarely) never
            // written yet. Either way there is nothing to reconcile against.
            // This is what actually stops a timer whose `fire` is racing
            // `archive_current_session` on another thread from recreating
            // the sidecar the archive just deleted; `armed` only serializes
            // `fire` against `Reminders::clear`, not against the archive
            // itself. See `session_file_exists`.
            return Ok(());
        }
        if armed.is_empty() {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("remove reminders: {error}"));
            }
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create reminder directory: {error}"))?;
        }
        let items: Vec<&StoredReminder> = armed.iter().map(|entry| &entry.item).collect();
        let body = serde_json::to_vec_pretty(&items)
            .map_err(|error| format!("encode reminders: {error}"))?;
        replace_private(path, &body)
    }

    /// Delivers the timer, unless it was cancelled while its sleep was
    /// waking. The notification is pushed into the inbox before the timer is
    /// removed from the sidecar file, so an exit between the two only risks
    /// firing this timer again on the next resume, never losing it.
    ///
    /// The `armed` lock is held across the find, the push, and the removal.
    /// `Reminders::cancel` also removes under this lock, so a cancel that
    /// runs concurrently with a fire either wins outright (the entry is gone
    /// before `fire` looks it up) or loses outright (the entry, and the
    /// notification, are already committed); it cannot land in between and
    /// let a cancelled timer still notify. `Inbox::push` takes only the
    /// inbox's own state lock and never locks `armed`, so holding `armed`
    /// here cannot deadlock against it.
    fn fire(&self, id: &str) {
        let mut armed = self.armed.lock().expect(STATE_MUTEX);
        let Some(index) = armed.iter().position(|entry| entry.item.id == id) else {
            return;
        };
        let message = armed[index].item.message.clone();
        self.inbox.push(Notification {
            task_id: "timer".into(),
            text: format!("[timer] {message}"),
            ..Notification::default()
        });
        armed.remove(index);
        let _ = self.save(&armed);
    }
}

/// Writes `body` to `path` with mode 0600 and `fsync`s it.
pub(crate) fn write_private(path: &Path, body: &[u8]) -> Result<(), String> {
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

/// Writes `body` to `path`'s `.tmp` sibling ([`write_private`]: mode 0600,
/// `fsync`d), renames it over `path`, then `fsync`s the containing directory.
///
/// A rename is only durable once its directory entry is itself synced: a
/// host crash right after a successful rename can still lose it otherwise.
/// Shared by the reminders sidecar (`<id>.reminders.json`, via
/// [`Inner::save`]) and the inbox sidecar (`<id>.inbox.json`, via
/// `write_inbox_file` in `cli::wiring`), so the temporary-file/fsync/rename
/// pattern exists in exactly one place.
pub(crate) fn replace_private(path: &Path, body: &[u8]) -> Result<(), String> {
    let mut tmp = path.to_path_buf();
    tmp.as_mut_os_string().push(".tmp");
    write_private(&tmp, body)?;
    std::fs::rename(&tmp, path).map_err(|error| format!("persist reminders: {error}"))?;
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync reminder directory: {error}"))
}

/// Whether the session `sidecar` belongs to still has its own `<id>.jsonl`
/// beside it, derived by stripping `suffix` (`.reminders.json` or
/// `.inbox.json`) from `sidecar`'s file name to recover `id`. `true` when the
/// name does not have the expected suffix, so a caller that is unsure never
/// blocks a write on that account.
///
/// `session::prepared::archive_open_file` renames the session's own file
/// into `archive/` before it removes the sidecars, so this is `false` for
/// the rest of that call and afterwards. Shared by the reminders sidecar
/// (via [`Inner::save`]) and the inbox sidecar (via `write_inbox_file` in
/// `cli::wiring`).
pub(crate) fn session_file_exists(sidecar: &Path, suffix: &str) -> bool {
    let Some(name) = sidecar.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    let Some(id) = name.strip_suffix(suffix) else {
        return true;
    };
    sidecar.with_file_name(format!("{id}.jsonl")).exists()
}

/// The session's outstanding timers: what is armed, what is on disk, and how
/// to stop one or all of them.
pub struct Reminders {
    inner: Arc<Inner>,
    /// The session-scoped parent of every timer's token.
    token: CancellationToken,
}

impl Reminders {
    pub fn new(inbox: Arc<Inbox>) -> Self {
        Self::create(inbox, None)
    }

    /// Restores any timers already stored at `path`, then keeps writing new
    /// ones there. A timer that is already due fires as soon as the runtime
    /// polls it.
    pub fn with_persist(inbox: Arc<Inbox>, path: PathBuf) -> Self {
        let stored: Vec<StoredReminder> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let reminders = Self::create(inbox, Some(path));
        reminders
            .inner
            .next_id
            .store(highest_id(&stored), Ordering::SeqCst);
        for item in stored.into_iter().take(MAX_OUTSTANDING) {
            let _ = reminders.arm(item, false);
        }
        reminders
    }

    fn create(inbox: Arc<Inbox>, path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                inbox,
                path,
                armed: Mutex::new(Vec::new()),
                next_id: AtomicUsize::new(0),
            }),
            token: CancellationToken::new(),
        }
    }

    /// The outstanding timers, earliest first.
    pub fn list(&self) -> Vec<StoredReminder> {
        let armed = self.inner.armed.lock().expect(STATE_MUTEX);
        let mut items: Vec<StoredReminder> = armed.iter().map(|entry| entry.item.clone()).collect();
        items.sort_by_key(|item| item.fire_at);
        items
    }

    /// Stops one timer: it never fires and is removed from the file.
    pub fn cancel(&self, id: &str) -> Result<StoredReminder, String> {
        let entry = {
            let mut armed = self.inner.armed.lock().expect(STATE_MUTEX);
            let index = armed
                .iter()
                .position(|entry| entry.item.id == id)
                .ok_or_else(|| format!("unknown timer: {id}"))?;
            let entry = armed.remove(index);
            let _ = self.inner.save(&armed);
            entry
        };
        entry.token.cancel();
        Ok(entry.item)
    }

    /// Stops every timer and removes the file. Archiving a session calls this
    /// so its timers cannot come back when the file is gone.
    pub fn clear(&self) {
        let drained = {
            let mut armed = self.inner.armed.lock().expect(STATE_MUTEX);
            let drained = std::mem::take(&mut *armed);
            let _ = self.inner.save(&armed);
            drained
        };
        for entry in drained {
            entry.token.cancel();
        }
    }

    /// Arms a new timer and records it. `pub(crate)` so lifecycle tests can
    /// put one in flight without going through the tool.
    pub(crate) fn schedule(
        &self,
        delay: Duration,
        message: String,
    ) -> Result<StoredReminder, String> {
        let id = format!("r{}", self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let fire_at = Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default();
        let item = StoredReminder {
            id,
            fire_at,
            message,
        };
        self.arm(item.clone(), true)?;
        Ok(item)
    }

    fn arm(&self, item: StoredReminder, write: bool) -> Result<(), String> {
        let token = self.token.child_token();
        {
            let mut armed = self.inner.armed.lock().expect(STATE_MUTEX);
            if armed.len() >= MAX_OUTSTANDING {
                return Err(format!("too many reminders (max {MAX_OUTSTANDING})"));
            }
            armed.push(Armed {
                item: item.clone(),
                token: token.clone(),
            });
            if write && let Err(error) = self.inner.save(&armed) {
                armed.pop();
                return Err(error);
            }
        }
        let delay = delay_until(item.fire_at);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(delay) => inner.fire(&item.id),
                () = token.cancelled() => {}
            }
        });
        Ok(())
    }
}

impl Drop for Reminders {
    fn drop(&mut self) {
        self.token.cancel();
        // The file stays, so resuming the session re-arms from it. Emptying
        // the list makes `Inner::fire` a no-op for a timer whose sleep and
        // cancellation complete in the same moment.
        if let Ok(mut armed) = self.inner.armed.lock() {
            armed.clear();
        }
    }
}

/// The highest `r{n}` a restored file already used, so the session does not
/// hand out an id that is still outstanding.
fn highest_id(items: &[StoredReminder]) -> usize {
    items
        .iter()
        .filter_map(|item| item.id.strip_prefix('r')?.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
}

fn delay_until(fire_at: DateTime<Utc>) -> Duration {
    (fire_at - Utc::now()).to_std().unwrap_or(Duration::ZERO)
}

/// One timer in the layout shared by `remind_status` and `/timers`: id, the
/// wait until it fires (`due` once that time has passed), and the message.
pub fn timer_line(item: &StoredReminder, now: DateTime<Utc>) -> String {
    let remaining = item.fire_at - now;
    let when = if remaining <= TimeDelta::zero() {
        "due".to_string()
    } else {
        format!("in {}", round_to_seconds(remaining))
    };
    format!("{:<4} {when:>10}  {}", item.id, item.message)
}

/// The three timer definitions, without a built [`Reminders`]. The redaction
/// boundary check needs the tool set a real session would register.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        remind_definition(),
        remind_status_definition(),
        remind_cancel_definition(),
    ]
}

/// The parent-side timer tools in registration order.
pub fn tools(reminders: &Arc<Reminders>) -> Vec<Box<dyn Tool + Send + Sync>> {
    vec![
        Box::new(RemindTool {
            reminders: Arc::clone(reminders),
        }),
        Box::new(RemindStatusTool {
            reminders: Arc::clone(reminders),
        }),
        Box::new(RemindCancelTool {
            reminders: Arc::clone(reminders),
        }),
    ]
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

/// The schema advertised for `remind_status`.
pub fn remind_status_definition() -> ToolDefinition {
    definition(
        "remind_status",
        STATUS_DESCRIPTION,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    )
}

/// The schema advertised for `remind_cancel`.
pub fn remind_cancel_definition() -> ToolDefinition {
    definition(
        "remind_cancel",
        CANCEL_DESCRIPTION,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The timer id, as remind_status lists it."
                }
            },
            "required": ["id"]
        }),
    )
}

/// Schedules timer notifications into the session inbox.
pub struct RemindTool {
    reminders: Arc<Reminders>,
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
        match self.reminders.schedule(
            Duration::from_secs(args.seconds as u64),
            message.to_string(),
        ) {
            Ok(item) => text_result(format!(
                "scheduled {} in {}s: {message}",
                item.id, args.seconds
            )),
            Err(error) => error_result(error),
        }
    }
}

/// Lists the session's outstanding timers.
pub struct RemindStatusTool {
    reminders: Arc<Reminders>,
}

#[async_trait::async_trait]
impl Tool for RemindStatusTool {
    fn definition(&self) -> ToolDefinition {
        remind_status_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if let Err(message) = decode_strict_json::<RemindStatusArgs>(arguments.get(), &[]) {
            return error_result(message);
        }
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let items = self.reminders.list();
        if items.is_empty() {
            return text_result(NO_TIMERS);
        }
        let now = Utc::now();
        let lines: Vec<String> = items.iter().map(|item| timer_line(item, now)).collect();
        text_result(lines.join("\n"))
    }
}

/// Cancels one of the session's outstanding timers.
pub struct RemindCancelTool {
    reminders: Arc<Reminders>,
}

#[async_trait::async_trait]
impl Tool for RemindCancelTool {
    fn definition(&self) -> ToolDefinition {
        remind_cancel_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: RemindCancelArgs = match decode_strict_json(arguments.get(), &["id"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        match self.reminders.cancel(args.id.trim()) {
            Ok(item) => text_result(format!("canceled {}: {}", item.id, item.message)),
            Err(error) => error_result(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::tool::testutil::{run, run_cancelled};

    fn tool_and_inbox() -> (RemindTool, Arc<Reminders>, Arc<Inbox>) {
        let inbox = Arc::new(Inbox::default());
        let reminders = Arc::new(Reminders::new(Arc::clone(&inbox)));
        (
            RemindTool {
                reminders: Arc::clone(&reminders),
            },
            reminders,
            inbox,
        )
    }

    fn persist_setup() -> (tempfile::TempDir, PathBuf, Arc<Inbox>) {
        let directory = tempfile::tempdir().expect("temp dir");
        // `Inner::save` skips writing once `session.jsonl` is gone (the
        // archive-race guard), so a stand-in for it must exist beside the
        // sidecar for these tests to exercise ordinary persistence.
        std::fs::write(directory.path().join("session.jsonl"), b"").expect("stub session file");
        let path = directory.path().join("session.reminders.json");
        (directory, path, Arc::new(Inbox::default()))
    }

    fn persisted(path: &Path) -> Vec<StoredReminder> {
        let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "[]".to_string());
        serde_json::from_str(&raw).expect("stored reminders")
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
        let (tool, reminders, inbox) = tool_and_inbox();

        let result = run(&tool, r#"{"seconds":10,"message":"check the tests"}"#).await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "scheduled r1 in 10s: check the tests");
        assert!(inbox.is_empty(), "the timer must not fire before the delay");

        reminders
            .schedule(Duration::ZERO, "check the tests".into())
            .expect("zero-delay timer");
        wait_until_fired(&inbox).await;

        let items = inbox.drain();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].task_id, "timer");
        assert_eq!(items[0].text, "[timer] check the tests");
    }

    #[tokio::test]
    async fn dropping_the_registry_cancels_an_unfired_timer() {
        let inbox = Arc::new(Inbox::default());
        {
            let reminders = Reminders::new(Arc::clone(&inbox));
            reminders
                .schedule(Duration::ZERO, "gone".into())
                .expect("schedule");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(inbox.is_empty(), "a dropped registry must not fire");
    }

    #[tokio::test]
    async fn invalid_arguments_are_rejected() {
        let (tool, _reminders, inbox) = tool_and_inbox();
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
        let (tool, _reminders, _inbox) = tool_and_inbox();
        for index in 0..MAX_OUTSTANDING {
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
        let (tool, _reminders, inbox) = tool_and_inbox();
        let result = run_cancelled(&tool, r#"{"seconds":1,"message":"nope"}"#).await;
        assert!(result.is_error, "{result:?}");
        assert_eq!(result.content, CONTEXT_CANCELED);
        assert!(inbox.is_empty());
    }

    #[tokio::test]
    async fn a_schedule_is_written_beside_the_session() {
        let (_directory, path, inbox) = persist_setup();
        let reminders = Arc::new(Reminders::with_persist(Arc::clone(&inbox), path.clone()));
        let tool = RemindTool {
            reminders: Arc::clone(&reminders),
        };
        let result = run(&tool, r#"{"seconds":60,"message":"later"}"#).await;
        assert!(!result.is_error, "{result:?}");
        let stored = persisted(&path);
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(stored[0].message, "later");
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
        let _reminders = Reminders::with_persist(Arc::clone(&inbox), path.clone());
        wait_until_fired(&inbox).await;
        let items = inbox.drain();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].text, "[timer] due");
        assert!(persisted(&path).is_empty(), "{:?}", persisted(&path));
    }

    #[tokio::test]
    async fn the_notification_is_queued_before_the_timer_leaves_the_file() {
        let (_directory, path, _unused) = persist_setup();
        // The inbox's change callback runs synchronously inside `push`,
        // before `fire` goes on to remove the timer from the sidecar. If the
        // sidecar still lists the timer at that point, the push happened
        // first.
        let seen_on_disk_at_push = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Arc::clone(&seen_on_disk_at_push);
        let probe_path = path.clone();
        let inbox = Arc::new(Inbox::new(Some(Box::new(move || {
            let stored = persisted(&probe_path);
            if stored.iter().any(|item| item.message == "check ordering") {
                probe.store(true, Ordering::SeqCst);
            }
        }))));
        let reminders = Reminders::with_persist(Arc::clone(&inbox), path.clone());
        reminders
            .schedule(Duration::ZERO, "check ordering".into())
            .expect("schedule");

        wait_until_fired(&inbox).await;

        assert!(
            seen_on_disk_at_push.load(Ordering::SeqCst),
            "the timer must still be on disk when its notification is pushed"
        );
        assert!(
            persisted(&path).is_empty(),
            "the timer is removed once it has fired"
        );
    }

    #[tokio::test]
    async fn a_fire_after_the_session_file_is_gone_does_not_recreate_the_sidecar() {
        // Mirrors what `session::prepared::archive_open_file` leaves behind:
        // the session's own file gone, and its sidecars already removed. A
        // timer racing that archive on another thread must still notify the
        // in-process inbox (at-least-once delivery), but must not recreate
        // the sidecar file for a session that no longer exists.
        let (directory, path, inbox) = persist_setup();
        let reminders = Reminders::with_persist(Arc::clone(&inbox), path.clone());
        let item = reminders
            .schedule(Duration::from_secs(60), "late".into())
            .expect("schedule");
        assert!(
            path.exists(),
            "the sidecar exists while the session is live"
        );

        std::fs::remove_file(directory.path().join("session.jsonl")).expect("remove session file");
        let _ = std::fs::remove_file(&path);

        reminders.inner.fire(&item.id);

        assert!(
            !inbox.is_empty(),
            "the notification still reaches the in-process inbox"
        );
        assert!(
            !path.exists(),
            "a fire that races the archive must not recreate the sidecar"
        );
    }

    #[test]
    fn session_file_exists_checks_the_id_derived_from_the_sidecar_suffix() {
        let directory = tempfile::tempdir().expect("temp dir");
        let sidecar = directory.path().join("s1.reminders.json");

        assert!(
            !session_file_exists(&sidecar, ".reminders.json"),
            "no session file yet"
        );

        std::fs::write(directory.path().join("s1.jsonl"), b"").expect("write session file");
        assert!(
            session_file_exists(&sidecar, ".reminders.json"),
            "the session file now exists"
        );

        std::fs::remove_file(directory.path().join("s1.jsonl")).expect("remove session file");
        assert!(
            !session_file_exists(&sidecar, ".reminders.json"),
            "archiving removed it again"
        );
    }

    #[tokio::test]
    async fn dropping_the_registry_keeps_unfired_reminders_on_disk() {
        let (_directory, path, inbox) = persist_setup();
        {
            let reminders = Arc::new(Reminders::with_persist(Arc::clone(&inbox), path.clone()));
            let tool = RemindTool {
                reminders: Arc::clone(&reminders),
            };
            let result = run(&tool, r#"{"seconds":60,"message":"keep"}"#).await;
            assert!(!result.is_error, "{result:?}");
        }
        let stored = persisted(&path);
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(stored[0].message, "keep");
    }

    #[tokio::test]
    async fn a_restored_session_does_not_reuse_an_outstanding_id() {
        let (_directory, path, inbox) = persist_setup();
        std::fs::write(
            &path,
            r#"[{"id":"r3","fire_at":"2999-01-01T00:00:00Z","message":"restored"}]"#,
        )
        .expect("write");
        let reminders = Reminders::with_persist(Arc::clone(&inbox), path.clone());

        let fresh = reminders
            .schedule(Duration::from_secs(60), "new".into())
            .expect("schedule");

        assert_eq!(fresh.id, "r4");
        let ids: Vec<String> = reminders.list().into_iter().map(|item| item.id).collect();
        assert_eq!(ids, vec!["r4".to_string(), "r3".to_string()]);
    }

    #[tokio::test]
    async fn the_list_is_earliest_first() {
        let (_tool, reminders, _inbox) = tool_and_inbox();
        reminders
            .schedule(Duration::from_secs(60), "later".into())
            .expect("schedule");
        reminders
            .schedule(Duration::from_secs(5), "sooner".into())
            .expect("schedule");

        let messages: Vec<String> = reminders
            .list()
            .into_iter()
            .map(|item| item.message)
            .collect();

        assert_eq!(messages, vec!["sooner".to_string(), "later".to_string()]);
    }

    #[tokio::test]
    async fn cancelling_a_timer_stops_it_and_rewrites_the_file() {
        let (_directory, path, inbox) = persist_setup();
        let reminders = Reminders::with_persist(Arc::clone(&inbox), path.clone());
        reminders
            .schedule(Duration::from_secs(60), "keep".into())
            .expect("schedule");
        let doomed = reminders
            .schedule(Duration::ZERO, "stop me".into())
            .expect("schedule");

        let canceled = reminders.cancel(&doomed.id).expect("cancel");

        assert_eq!(canceled.message, "stop me");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(inbox.is_empty(), "a canceled timer must not fire");
        let stored = persisted(&path);
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(stored[0].message, "keep");
        assert_eq!(reminders.list().len(), 1);
    }

    #[tokio::test]
    async fn a_cancel_racing_a_fire_never_lets_both_win() {
        // `Inner::fire` and `Reminders::cancel` both remove the timer under
        // the same `armed` lock, so they must serialize: either the cancel
        // wins outright (no notification) or the fire wins outright (the
        // cancel sees "unknown timer"), never a canceled timer that still
        // notifies. Drive `fire` from a real OS thread against `cancel` on
        // this one, synchronized by a barrier, to force the two onto the
        // lock at the same instant instead of relying on `tokio::select!`'s
        // branch-selection timing.
        let (_directory, path, inbox) = persist_setup();
        let reminders = Reminders::with_persist(Arc::clone(&inbox), path.clone());
        let item = reminders
            .schedule(Duration::from_secs(60), "race".into())
            .expect("schedule");
        let id = item.id.clone();

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let fire_inner = Arc::clone(&reminders.inner);
        let fire_id = id.clone();
        let fire_barrier = Arc::clone(&barrier);
        let fire_thread = std::thread::spawn(move || {
            fire_barrier.wait();
            fire_inner.fire(&fire_id);
        });

        barrier.wait();
        let cancel_result = reminders.cancel(&id);
        fire_thread.join().expect("fire thread panicked");

        let notified = !inbox.is_empty();
        match cancel_result {
            Ok(_) => assert!(
                !notified,
                "cancel reported success but the timer also notified"
            ),
            Err(_) => assert!(
                notified,
                "fire won the race but left no notification behind"
            ),
        }
    }

    #[tokio::test]
    async fn cancelling_an_unknown_timer_reports_the_id() {
        let (_tool, reminders, _inbox) = tool_and_inbox();
        let error = reminders.cancel("r9").expect_err("unknown id");
        assert_eq!(error, "unknown timer: r9");
    }

    #[tokio::test]
    async fn clearing_stops_every_timer_and_removes_the_file() {
        let (_directory, path, inbox) = persist_setup();
        let reminders = Reminders::with_persist(Arc::clone(&inbox), path.clone());
        reminders
            .schedule(Duration::ZERO, "one".into())
            .expect("schedule");
        reminders
            .schedule(Duration::from_secs(60), "two".into())
            .expect("schedule");

        reminders.clear();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(inbox.is_empty(), "a cleared timer must not fire");
        assert!(reminders.list().is_empty());
        assert!(!path.exists(), "the sidecar must be gone");
    }

    #[tokio::test]
    async fn the_status_tool_lists_outstanding_timers() {
        let inbox = Arc::new(Inbox::default());
        let reminders = Arc::new(Reminders::new(Arc::clone(&inbox)));
        let status = RemindStatusTool {
            reminders: Arc::clone(&reminders),
        };

        let empty = run(&status, "{}").await;
        assert!(!empty.is_error, "{empty:?}");
        assert_eq!(empty.content, NO_TIMERS);

        reminders
            .schedule(Duration::from_secs(90), "check the build".into())
            .expect("schedule");
        let listed = run(&status, "{}").await;

        assert!(!listed.is_error, "{listed:?}");
        assert!(listed.content.starts_with("r1  "), "{}", listed.content);
        assert!(listed.content.contains("in 1m"), "{}", listed.content);
        assert!(
            listed.content.ends_with("  check the build"),
            "{}",
            listed.content
        );
    }

    #[tokio::test]
    async fn the_cancel_tool_stops_a_timer_by_id() {
        let inbox = Arc::new(Inbox::default());
        let reminders = Arc::new(Reminders::new(Arc::clone(&inbox)));
        let cancel = RemindCancelTool {
            reminders: Arc::clone(&reminders),
        };
        reminders
            .schedule(Duration::ZERO, "stop me".into())
            .expect("schedule");

        let result = run(&cancel, r#"{"id":"r1"}"#).await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "canceled r1: stop me");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(inbox.is_empty());

        let unknown = run(&cancel, r#"{"id":"r1"}"#).await;
        assert!(unknown.is_error, "{unknown:?}");
        assert_eq!(unknown.content, "unknown timer: r1");
    }

    #[test]
    fn a_timer_line_shows_the_wait_and_due_once_it_passes() {
        let now = DateTime::parse_from_rfc3339("2026-09-20T12:00:00Z")
            .expect("now")
            .with_timezone(&Utc);
        let item = StoredReminder {
            id: "r1".into(),
            fire_at: now + TimeDelta::seconds(252),
            message: "check the build".into(),
        };

        assert_eq!(timer_line(&item, now), "r1     in 4m12s  check the build");

        let overdue = StoredReminder {
            fire_at: now - TimeDelta::seconds(5),
            ..item
        };
        assert_eq!(
            timer_line(&overdue, now),
            "r1          due  check the build"
        );
    }
}
