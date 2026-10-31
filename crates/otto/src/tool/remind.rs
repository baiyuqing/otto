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
//! on disk; the three tools and [`crate::runtime::builder::Runner`] hold
//! `Arc`s of it, and `/timers` reads the runner's. Concurrency: `execute`
//! takes `&self` and may run concurrently, so every mutation goes through one
//! mutex. Cancellation: the *turn* token only aborts a call that has not yet
//! spawned; each timer sleeps on a child of the session token, so a finished
//! turn cannot kill it, [`Reminders::cancel`] stops one, and dropping the
//! registry stops all.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use otto_core::mailbox::{Mailbox, Notification};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::fsops::{self, PosixPublish, Publish, PublishMode, PublishOps, PublishStep};
use crate::subagent::format::round_to_seconds;

use super::result::decode_strict_json;
use super::{CONTEXT_CANCELED, Tool, definition, error_result, preflight_error, text_result};

const MIN_SECONDS: i64 = 1;
const MAX_SECONDS: i64 = 3600;
const MAX_MESSAGE_CHARS: usize = 500;
const MAX_OUTSTANDING: usize = 8;
const STATE_MUTEX: &str = "reminder state mutex";

/// Which side of the externally visible filesystem mutation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistPhase {
    BeforeEffect,
    AfterEffect,
}

#[derive(Debug)]
struct PersistError {
    phase: PersistPhase,
    message: String,
}

impl PersistError {
    fn before(message: impl Into<String>) -> Self {
        Self {
            phase: PersistPhase::BeforeEffect,
            message: message.into(),
        }
    }

    fn after(message: impl Into<String>) -> Self {
        Self {
            phase: PersistPhase::AfterEffect,
            message: message.into(),
        }
    }

    fn into_tool_result(self) -> ToolResult {
        match self.phase {
            PersistPhase::BeforeEffect => preflight_error(self.message),
            PersistPhase::AfterEffect => error_result(self.message).transport_unknown(),
        }
    }
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug)]
enum ReminderError {
    NotStarted(String),
    Persist(PersistError),
}

impl ReminderError {
    fn into_tool_result(self) -> ToolResult {
        match self {
            Self::NotStarted(message) => preflight_error(message),
            Self::Persist(error) => error.into_tool_result(),
        }
    }
}

impl std::fmt::Display for ReminderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted(message) => formatter.write_str(message),
            Self::Persist(error) => error.fmt(formatter),
        }
    }
}
/// What `remind_status` and `/timers` print for an empty registry, in the
/// shape `agent_status` and `/tasks` use for theirs.
pub const NO_TIMERS: &str = "no timers in this session";

const DESCRIPTION: &str = "Schedule a reminder that arrives later as a [timer] message and starts \
    a wake turn. Returns immediately; does not block. Timers are kept with \
    the session across a restart (at most 8 at once, 1 to 3600 seconds); \
    /new starts a fresh session without them, and archiving a session \
    cancels them. Use this when you need to continue after a delay without \
    waiting in the current turn. It is your own timer inside this \
    conversation; for results the user asked to receive later or \
    repeatedly, use schedule. Use remind_status to see what is outstanding \
    and remind_cancel to stop one.";

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
    mailbox: Arc<dyn Mailbox>,
    /// `None` for `--no-session`, which has nothing to write.
    path: Option<PathBuf>,
    armed: Mutex<Vec<Armed>>,
    next_id: AtomicUsize,
    publish: Arc<dyn PublishOps>,
}

impl Inner {
    /// Mirrors `armed` to disk. The caller holds the lock and passes the list
    /// so the file never disagrees with the registry.
    fn save(&self, armed: &[Armed]) -> Result<(), PersistError> {
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
            let removed = match std::fs::remove_file(path) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(PersistError::before(format!("remove reminders: {error}")));
                }
            };
            let synced = fsops::open_parent(path)
                .and_then(|(directory, name)| self.publish.sync_dir(&directory, name));
            return synced.map_err(|error| {
                let message = format!("sync reminder directory: {error}");
                if removed {
                    PersistError::after(message)
                } else {
                    PersistError::before(message)
                }
            });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                PersistError::before(format!("create reminder directory: {error}"))
            })?;
        }
        let items: Vec<&StoredReminder> = armed.iter().map(|entry| &entry.item).collect();
        let body = serde_json::to_vec_pretty(&items)
            .map_err(|error| PersistError::before(format!("encode reminders: {error}")))?;
        replace_private_typed(path, &body, self.publish.as_ref())
    }

    /// Delivers the timer, unless it was cancelled while its sleep was
    /// waking. The notification is pushed into the inbox before the timer is
    /// removed from the sidecar file, so an exit between the two only risks
    /// firing this timer again on the next resume, never losing it; its id,
    /// derived from the timer id and fire time, keeps that second firing from
    /// being queued or delivered twice.
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
        let item = armed[index].item.clone();
        let persisted = self
            .mailbox
            .enqueue(Notification {
                task_id: "timer".into(),
                text: format!("[timer] {}", item.message),
                id: format!("remind:{}:{}", item.id, item.fire_at.timestamp_millis()),
                ..Notification::default()
            })
            .is_ok();
        armed.remove(index);
        // A notification that did not reach the mailbox sidecar keeps its
        // timer in the reminders file, so a restart fires it again; the
        // stable id keeps that from delivering it twice.
        if persisted {
            let _ = self.save(&armed);
        }
    }
}

/// Publishes `body` as `path` with mode 0600 through
/// [`fsops::publish_file`]: write the `.tmp` sibling, `fsync` it, rename it
/// over `path`, then `fsync` the containing directory.
///
/// A rename is only durable once its directory entry is itself synced: a
/// host crash right after a successful rename can still lose it otherwise.
/// Shared by the reminders sidecar (`<id>.reminders.json`, via
/// [`Inner::save`]) and the inbox sidecar (`<id>.inbox.json`, via
/// `write_inbox_file` in `runtime::wiring`), so the temporary-file/fsync/rename
/// pattern exists in exactly one place.
///
/// The publish system calls are supplied so tests can fail or end the process
/// at one; production callers pass `&PosixPublish`.
pub(crate) fn replace_private_with(
    path: &Path,
    body: &[u8],
    publish: &dyn PublishOps,
) -> Result<(), String> {
    replace_private_typed(path, body, publish).map_err(|error| error.to_string())
}

fn replace_private_typed(
    path: &Path,
    body: &[u8],
    publish: &dyn PublishOps,
) -> Result<(), PersistError> {
    let write_error =
        |error: std::io::Error| PersistError::before(format!("write reminders: {error}"));
    let (directory, name) = fsops::open_parent(path).map_err(write_error)?;
    let temp_name = format!("{name}.tmp");
    // A temporary file left by an earlier crash: `publish_file` creates the
    // temporary file exclusively, so it is removed first.
    match fsops::unlink_at(&directory, &temp_name) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(write_error(error));
        }
        _ => {}
    }
    fsops::publish_file(
        &directory,
        &Publish {
            name,
            temp: &temp_name,
            permissions: 0o600,
            mode: PublishMode::Replace,
        },
        body,
        publish,
    )
    .map_err(|error| match error.step {
        PublishStep::CreateTemp | PublishStep::Write => {
            PersistError::before(format!("write reminders: {}", error.source))
        }
        PublishStep::FileSync => PersistError::before(format!("sync reminders: {}", error.source)),
        PublishStep::Rename => PersistError::before(format!("persist reminders: {}", error.source)),
        PublishStep::DirSync => {
            PersistError::after(format!("sync reminder directory: {}", error.source))
        }
    })
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
/// `runtime::wiring`).
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
    pub fn new(mailbox: Arc<dyn Mailbox>) -> Self {
        Self::create(mailbox, None)
    }

    /// Restores any timers already stored at `path`, then keeps writing new
    /// ones there. A timer that is already due fires as soon as the runtime
    /// polls it.
    pub fn with_persist(mailbox: Arc<dyn Mailbox>, path: PathBuf) -> Self {
        let stored: Vec<StoredReminder> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let reminders = Self::create(mailbox, Some(path));
        reminders
            .inner
            .next_id
            .store(highest_id(&stored), Ordering::SeqCst);
        for item in stored.into_iter().take(MAX_OUTSTANDING) {
            let _ = reminders.arm(item, false);
        }
        reminders
    }

    fn create(mailbox: Arc<dyn Mailbox>, path: Option<PathBuf>) -> Self {
        Self::create_with_publish_ops(mailbox, path, Arc::new(PosixPublish))
    }

    fn create_with_publish_ops(
        mailbox: Arc<dyn Mailbox>,
        path: Option<PathBuf>,
        publish: Arc<dyn PublishOps>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                mailbox,
                path,
                armed: Mutex::new(Vec::new()),
                next_id: AtomicUsize::new(0),
                publish,
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
        self.cancel_typed(id).map_err(|error| error.to_string())
    }

    fn cancel_typed(&self, id: &str) -> Result<StoredReminder, ReminderError> {
        let entry = {
            let mut armed = self.inner.armed.lock().expect(STATE_MUTEX);
            let index = armed
                .iter()
                .position(|entry| entry.item.id == id)
                .ok_or_else(|| ReminderError::NotStarted(format!("unknown timer: {id}")))?;
            let entry = armed.remove(index);
            if let Err(error) = self.inner.save(&armed) {
                armed.insert(index, entry);
                return Err(ReminderError::Persist(error));
            }
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
    #[cfg(test)]
    pub(crate) fn schedule(
        &self,
        delay: Duration,
        message: String,
    ) -> Result<StoredReminder, String> {
        self.schedule_typed(delay, message)
            .map_err(|error| error.to_string())
    }

    fn schedule_typed(
        &self,
        delay: Duration,
        message: String,
    ) -> Result<StoredReminder, ReminderError> {
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

    fn arm(&self, item: StoredReminder, write: bool) -> Result<(), ReminderError> {
        let token = self.token.child_token();
        {
            let mut armed = self.inner.armed.lock().expect(STATE_MUTEX);
            if armed.len() >= MAX_OUTSTANDING {
                return Err(ReminderError::NotStarted(format!(
                    "too many reminders (max {MAX_OUTSTANDING})"
                )));
            }
            armed.push(Armed {
                item: item.clone(),
                token: token.clone(),
            });
            if write && let Err(error) = self.inner.save(&armed) {
                armed.pop();
                return Err(ReminderError::Persist(error));
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

impl Reminders {
    /// Stops every armed timer without touching the file, so resuming the
    /// session re-arms from it. Emptying the list makes `Inner::fire` a no-op
    /// for a timer whose sleep and cancellation complete in the same moment.
    /// Idempotent; also runs on drop.
    pub(crate) fn stop(&self) {
        self.token.cancel();
        if let Ok(mut armed) = self.inner.armed.lock() {
            armed.clear();
        }
    }
}

impl Drop for Reminders {
    fn drop(&mut self) {
        self.stop();
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

pub(crate) const REMIND_RETRY: (&str, otto_core::model::RetrySafety) =
    ("remind", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for RemindTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        REMIND_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        remind_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: RemindArgs = match decode_strict_json(arguments.get(), &["seconds", "message"]) {
            Ok(args) => args,
            Err(message) => return preflight_error(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED).cancelled_not_started();
        }
        let message = args.message.trim();
        if message.is_empty() {
            return preflight_error("message is required");
        }
        if message.chars().count() > MAX_MESSAGE_CHARS {
            return preflight_error(format!(
                "message is longer than {MAX_MESSAGE_CHARS} characters"
            ));
        }
        if args.seconds < MIN_SECONDS || args.seconds > MAX_SECONDS {
            return preflight_error(format!(
                "seconds must be between {MIN_SECONDS} and {MAX_SECONDS}"
            ));
        }
        match self.reminders.schedule_typed(
            Duration::from_secs(args.seconds as u64),
            message.to_string(),
        ) {
            Ok(item) => text_result(format!(
                "scheduled {} in {}s: {message}",
                item.id, args.seconds
            )),
            Err(error) => error.into_tool_result(),
        }
    }
}

/// Lists the session's outstanding timers.
pub struct RemindStatusTool {
    reminders: Arc<Reminders>,
}

pub(crate) const REMIND_STATUS_RETRY: (&str, otto_core::model::RetrySafety) =
    ("remind_status", otto_core::model::RetrySafety::ReadOnly);

#[async_trait::async_trait]
impl Tool for RemindStatusTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        REMIND_STATUS_RETRY.1
    }

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

pub(crate) const REMIND_CANCEL_RETRY: (&str, otto_core::model::RetrySafety) = (
    "remind_cancel",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait::async_trait]
impl Tool for RemindCancelTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        REMIND_CANCEL_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        remind_cancel_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: RemindCancelArgs = match decode_strict_json(arguments.get(), &["id"]) {
            Ok(args) => args,
            Err(message) => return preflight_error(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED).cancelled_not_started();
        }
        match self.reminders.cancel_typed(args.id.trim()) {
            Ok(item) => text_result(format!("canceled {}: {}", item.id, item.message)),
            Err(error) => error.into_tool_result(),
        }
    }
}

#[cfg(test)]
mod tests;
