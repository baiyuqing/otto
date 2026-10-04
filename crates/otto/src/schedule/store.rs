//! `~/.otto/schedule.db`: scheduled items and delivered occurrences, shared by
//! every otto process on the machine.
//!
//! Ownership: [`Store`] owns the connection behind one mutex, like
//! [`crate::subagent::record::Store`]. Every read-modify-write runs in one
//! `BEGIN IMMEDIATE` transaction (the [`crate::storage`] adapter's
//! `unchecked_transaction`), so claims are exclusive across processes.
//!
//! State machine of the current occurrence of an item (the spec has the
//! table): `waiting` -> `ready` (result text known) -> delivered, which
//! advances a recurring item to its next occurrence or sets `done`. An
//! occurrence is identified by `(id, due_at)`; results reported for an
//! occurrence that is no longer current, or for a cancelled item, are
//! ignored. All times are unix milliseconds compared with the caller's `now`.
//!
//! Errors: [`Error`] carries no database text. [`Store::open`] fails closed on
//! an unknown `user_version`.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveTime, TimeDelta, Utc};
use tokio::sync::Notify;

use super::time::{format_duration, format_local, latest_occurrence, next_occurrence};
use super::{Kind, Origin, Repeat, RunOutcome, Scope, State};
use crate::storage::{Connection, params};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY,
    origin TEXT NOT NULL,
    session_id TEXT NOT NULL,
    workspace TEXT NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    repeat TEXT NOT NULL,
    local_time TEXT NOT NULL,
    due_at INTEGER NOT NULL,
    state TEXT NOT NULL,
    result TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    next_try_at INTEGER NOT NULL,
    claimed_until INTEGER NOT NULL,
    last_error TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS items_origin ON items(origin, state);
CREATE TABLE IF NOT EXISTS deliveries (
    item_id INTEGER NOT NULL,
    due_at INTEGER NOT NULL,
    origin TEXT NOT NULL,
    session_id TEXT NOT NULL,
    text TEXT NOT NULL,
    delivered_at INTEGER NOT NULL,
    appended INTEGER NOT NULL,
    PRIMARY KEY (item_id, due_at)
) STRICT;
CREATE INDEX IF NOT EXISTS deliveries_pending ON deliveries(appended, delivered_at);
"#;

const COLUMNS: &str = "id, origin, session_id, workspace, kind, body, repeat, local_time, due_at, \
state, result, attempts, next_try_at, claimed_until, last_error, created_at, updated_at";

/// Active items (`waiting` or `ready`) one origin may hold.
const MAX_ACTIVE_PER_ORIGIN: i64 = 32;
const RUN_LEASE: Duration = Duration::from_secs(15 * 60);
/// An occurrence not delivered this long after its due time is failed.
const EXPIRY: TimeDelta = TimeDelta::hours(24);
const RETENTION: TimeDelta = TimeDelta::days(7);
const LATE_AFTER: TimeDelta = TimeDelta::minutes(2);
const BACKOFF_BASE_SECONDS: i64 = 30;
const BACKOFF_CAP_SECONDS: i64 = 30 * 60;
const MAX_ERROR_CHARS: usize = 300;
const EXPIRED: &str = "not delivered within 24 h of the due time";

/// A storage failure carries no database text across the adapter boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("schedule store unavailable")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
type Db<T> = crate::storage::Result<T>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// The origin already has 32 active items.
    Limit,
    Store(Error),
}

impl fmt::Display for CreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit => write!(
                formatter,
                "at most {MAX_ACTIVE_PER_ORIGIN} scheduled items may be active for this chat or surface; cancel one with schedule_cancel first"
            ),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CreateError {}

pub struct NewItem {
    pub origin: Origin,
    pub session_id: String,
    pub workspace: String,
    pub kind: Kind,
    pub body: String,
    pub repeat: Option<Repeat>,
    pub local_time: Option<NaiveTime>,
    pub due_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub origin: Origin,
    pub session_id: String,
    pub workspace: String,
    pub kind: Kind,
    pub body: String,
    pub repeat: Option<Repeat>,
    pub local_time: Option<NaiveTime>,
    pub due_at: DateTime<Utc>,
    pub state: State,
    pub result: String,
    pub attempts: i64,
    pub next_try_at: DateTime<Utc>,
    pub claimed_until: DateTime<Utc>,
    pub last_error: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The row id of a label such as `s7`, the inverse of [`Item::label`].
pub fn parse_label(label: &str) -> Option<i64> {
    label
        .strip_prefix('s')
        .and_then(|digits| digits.parse::<i64>().ok())
        .filter(|number| *number > 0)
}

impl Item {
    /// The id as the model sees it, `s7`.
    pub fn label(&self) -> String {
        format!("s{}", self.id)
    }

    /// The repeat rule and time of day, when both are stored.
    fn recurrence(&self) -> Option<(Repeat, NaiveTime)> {
        Some((self.repeat?, self.local_time?))
    }

    fn from_sql(row: &crate::storage::Row) -> Db<Self> {
        let bad = |what: &str| crate::storage::Error::ConversionFailure(format!("bad {what}"));
        let origin: String = row.get(1)?;
        let kind: String = row.get(4)?;
        let repeat: String = row.get(6)?;
        let local_time: String = row.get(7)?;
        let state: String = row.get(9)?;
        Ok(Self {
            id: row.get(0)?,
            origin: Origin::parse(&origin).map_err(|_| bad("origin"))?,
            session_id: row.get(2)?,
            workspace: row.get(3)?,
            kind: Kind::parse(&kind).ok_or_else(|| bad("kind"))?,
            body: row.get(5)?,
            repeat: if repeat.is_empty() {
                None
            } else {
                Some(Repeat::parse(&repeat).ok_or_else(|| bad("repeat"))?)
            },
            local_time: if local_time.is_empty() {
                None
            } else {
                Some(NaiveTime::parse_from_str(&local_time, "%H:%M").map_err(|_| bad("time"))?)
            },
            due_at: from_ms(row.get(8)?),
            state: State::parse(&state).ok_or_else(|| bad("state"))?,
            result: row.get(10)?,
            attempts: row.get(11)?,
            next_try_at: from_ms(row.get(12)?),
            claimed_until: from_ms(row.get(13)?),
            last_error: row.get(14)?,
            created_at: from_ms(row.get(15)?),
            updated_at: from_ms(row.get(16)?),
        })
    }
}

/// One delivered occurrence, kept until it is written back into a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub item_id: i64,
    pub due_at: DateTime<Utc>,
    pub origin: Origin,
    pub session_id: String,
    pub text: String,
    pub delivered_at: DateTime<Utc>,
}

impl Delivery {
    /// The context entry appended to a session:
    /// `[scheduled s7, due 2026-10-05 08:00 +08:00]\n<text>`.
    pub fn context_text(&self) -> String {
        format!(
            "[scheduled s{}, due {}]\n{}",
            self.item_id,
            format_local(self.due_at),
            self.text
        )
    }
}

fn ms(at: DateTime<Utc>) -> i64 {
    at.timestamp_millis()
}

fn from_ms(value: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(value).unwrap_or_default()
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}...")
    }
}

/// `30 s * 2^(attempts-1)`, capped at 30 min. `attempts` counts failures so far.
fn backoff(attempts: i64) -> TimeDelta {
    let exponent = (attempts - 1).clamp(0, 16) as u32;
    let seconds = (BACKOFF_BASE_SECONDS << exponent).min(BACKOFF_CAP_SECONDS);
    TimeDelta::seconds(seconds)
}

/// The result plus a late line when delivery is more than 2 min after the due time.
pub fn delivery_text(item: &Item, now: DateTime<Utc>) -> String {
    let late = now - item.due_at;
    if late > LATE_AFTER {
        format!(
            "{}\n(scheduled for {}, delivered {} late)",
            item.result,
            format_local(item.due_at),
            format_duration(late)
        )
    } else {
        item.result.clone()
    }
}

/// The first user message of a scheduled run.
pub fn run_text(item: &Item, now: DateTime<Utc>) -> String {
    format!(
        "[scheduled run {}: created {} from {}; due {}; now {}. No user is present; approval requests are denied.]\n\n{}",
        item.label(),
        format_local(item.created_at),
        item.origin.describe(),
        format_local(item.due_at),
        format_local(now),
        item.body
    )
}

pub struct Store {
    connection: Mutex<Connection>,
    notify: Notify,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error)?;
        }
        let connection = Connection::open(path).map_err(|_| Error)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| Error)?;
        connection
            .pragma_update("journal_mode", "WAL")
            .map_err(|_| Error)?;
        Self::initialize(connection)
    }

    /// Makes every later call fail with [`Error`], by poisoning the connection
    /// mutex, to test the callers' error paths.
    #[cfg(test)]
    pub(crate) fn poison_for_test(self: &std::sync::Arc<Self>) {
        let store = std::sync::Arc::clone(self);
        let _ = std::thread::spawn(move || {
            let _guard = store.connection.lock().unwrap();
            panic!("poison the schedule store");
        })
        .join();
    }

    /// A private in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory().map_err(|_| Error)?)
    }

    fn initialize(connection: Connection) -> Result<Self> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .map_err(|_| Error)?;
        match version {
            0 => {
                connection.execute_batch(SCHEMA).map_err(|_| Error)?;
                connection
                    .pragma_update("user_version", SCHEMA_VERSION)
                    .map_err(|_| Error)?;
            }
            version if version == SCHEMA_VERSION => {}
            _ => return Err(Error),
        }
        Ok(Self {
            connection: Mutex::new(connection),
            notify: Notify::new(),
        })
    }

    /// Wakes in-process workers after an item is created or cancelled.
    pub fn notifier(&self) -> &Notify {
        &self.notify
    }

    /// Runs `body` in one `BEGIN IMMEDIATE` transaction.
    fn write<T>(&self, body: impl FnOnce(&Connection) -> Db<T>) -> Result<T> {
        let connection = self.connection.lock().map_err(|_| Error)?;
        let transaction = connection.unchecked_transaction().map_err(|_| Error)?;
        let value = body(&transaction).map_err(|_| Error)?;
        transaction.commit().map_err(|_| Error)?;
        Ok(value)
    }

    fn read<T>(&self, body: impl FnOnce(&Connection) -> Db<T>) -> Result<T> {
        let connection = self.connection.lock().map_err(|_| Error)?;
        body(&connection).map_err(|_| Error)
    }

    pub fn create(
        &self,
        item: NewItem,
        now: DateTime<Utc>,
    ) -> std::result::Result<Item, CreateError> {
        let id = self
            .write(|connection| {
                let active: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM items WHERE origin = ?1 AND state IN ('waiting','ready')",
                    params![item.origin.as_str()],
                    |row| row.get(0),
                )?;
                if active >= MAX_ACTIVE_PER_ORIGIN {
                    return Ok(None);
                }
                connection.execute(
                    "INSERT INTO items (origin, session_id, workspace, kind, body, repeat, \
                     local_time, due_at, state, result, attempts, next_try_at, claimed_until, \
                     last_error, created_at, updated_at) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'waiting','',0,?8,0,'',?9,?9)",
                    params![
                        item.origin.as_str(),
                        item.session_id,
                        item.workspace,
                        item.kind.as_str(),
                        item.body,
                        item.repeat.map_or("", Repeat::as_str),
                        item.local_time
                            .map(|time| time.format("%H:%M").to_string())
                            .unwrap_or_default(),
                        ms(item.due_at),
                        ms(now),
                    ],
                )?;
                let id: i64 =
                    connection.query_row("SELECT last_insert_rowid()", (), |row| row.get(0))?;
                Ok(Some(id))
            })
            .map_err(CreateError::Store)?
            .ok_or(CreateError::Limit)?;
        let created = self
            .read(|connection| get(connection, id))
            .map_err(CreateError::Store)?
            .ok_or(CreateError::Store(Error))?;
        self.notify.notify_waiters();
        Ok(created)
    }

    /// Active items, then items that ended within 7 days.
    pub fn list(&self, origin: &Origin, now: DateTime<Utc>) -> Result<Vec<Item>> {
        self.read(|connection| {
            query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE origin = ?1 \
                     AND (state IN ('waiting','ready') OR updated_at > ?2) \
                     ORDER BY CASE WHEN state IN ('waiting','ready') THEN 0 ELSE 1 END, due_at, id"
                ),
                params![origin.as_str(), ms(now - RETENTION)],
            )
        })
    }

    /// `None` when `origin` has no active item `id`.
    pub fn cancel(&self, origin: &Origin, id: i64, now: DateTime<Utc>) -> Result<Option<Item>> {
        let cancelled = self.write(|connection| {
            let changed = connection.execute(
                "UPDATE items SET state = 'cancelled', claimed_until = 0, updated_at = ?3 \
                 WHERE id = ?1 AND origin = ?2 AND state IN ('waiting','ready')",
                params![id, origin.as_str(), ms(now)],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            get(connection, id)
        })?;
        self.notify.notify_waiters();
        Ok(cancelled)
    }

    /// Expiry (24 h), recurring catch-up and advance, retention (7 days).
    /// Rows whose claim has not expired are left alone.
    pub fn sweep(&self, now: DateTime<Utc>) -> Result<()> {
        self.write(|connection| {
            let due = query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE state IN ('waiting','ready') \
                     AND due_at <= ?1 AND claimed_until <= ?1"
                ),
                params![ms(now)],
            )?;
            for item in due {
                let age = now - item.due_at;
                match item.recurrence() {
                    None if age > EXPIRY => {
                        connection.execute(
                            "UPDATE items SET state = 'failed', last_error = ?2, updated_at = ?3 \
                             WHERE id = ?1",
                            params![item.id, EXPIRED, ms(now)],
                        )?;
                    }
                    None => {}
                    Some((repeat, time)) if item.state == State::Waiting => {
                        let latest = latest_occurrence(repeat, time, now, &Local);
                        if now - latest > EXPIRY {
                            advance(connection, &item, now, EXPIRED)?;
                        } else if latest > item.due_at {
                            connection.execute(
                                "UPDATE items SET due_at = ?2, next_try_at = ?2, attempts = 0, \
                                 updated_at = ?3 WHERE id = ?1",
                                params![item.id, ms(latest), ms(now)],
                            )?;
                        }
                    }
                    Some(_) if age > EXPIRY => advance(connection, &item, now, EXPIRED)?,
                    Some(_) => {}
                }
            }
            connection.execute(
                "DELETE FROM items WHERE state IN ('done','failed','cancelled') AND updated_at < ?1",
                params![ms(now - RETENTION)],
            )?;
            connection.execute(
                "DELETE FROM deliveries WHERE delivered_at < ?1",
                params![ms(now - RETENTION)],
            )?;
            Ok(())
        })
    }

    /// Claims one due `waiting` prompt item for `scope` (and `workspace`, when
    /// given) for 15 min.
    pub fn claim_run(
        &self,
        scope: &Scope,
        workspace: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<Item>> {
        self.write(|connection| {
            let candidates = query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE state = 'waiting' AND kind = 'prompt' \
                     AND next_try_at <= ?1 AND claimed_until <= ?1 \
                     AND (?3 = 0 OR workspace = ?2) ORDER BY next_try_at, id LIMIT 50"
                ),
                params![
                    ms(now),
                    workspace.unwrap_or(""),
                    i64::from(workspace.is_some())
                ],
            )?;
            let Some(mut item) = candidates
                .into_iter()
                .find(|item| scope.matches(&item.origin))
            else {
                return Ok(None);
            };
            item.claimed_until = now + RUN_LEASE;
            connection.execute(
                "UPDATE items SET claimed_until = ?2 WHERE id = ?1",
                params![item.id, ms(item.claimed_until)],
            )?;
            Ok(Some(item))
        })
    }

    /// Records the result of the run of occurrence `(id, due_at)`. `false`
    /// when that occurrence is no longer current or the item was cancelled.
    pub fn finish_run(
        &self,
        id: i64,
        due_at: DateTime<Utc>,
        outcome: RunOutcome,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.write(|connection| {
            let Some(item) = current(connection, id, due_at, State::Waiting)? else {
                return Ok(false);
            };
            let failure = match outcome {
                RunOutcome::Reply(reply) => {
                    let reply = reply.trim();
                    let result = if reply.is_empty() {
                        format!(
                            "⏰ Scheduled prompt {} finished with no reply.",
                            item.label()
                        )
                    } else {
                        format!("⏰ {reply}")
                    };
                    make_ready(connection, id, &result, "", now)?;
                    return Ok(true);
                }
                RunOutcome::FailedBeforeTools(error) if now - item.due_at < EXPIRY => {
                    let attempts = item.attempts + 1;
                    connection.execute(
                        "UPDATE items SET attempts = ?2, next_try_at = ?3, last_error = ?4, \
                         claimed_until = 0, updated_at = ?5 WHERE id = ?1",
                        params![
                            id,
                            attempts,
                            ms(now + backoff(attempts)),
                            cut(&error, MAX_ERROR_CHARS),
                            ms(now)
                        ],
                    )?;
                    return Ok(true);
                }
                RunOutcome::FailedBeforeTools(error) | RunOutcome::Failed(error) => {
                    cut(&error, MAX_ERROR_CHARS)
                }
            };
            let result = format!("⏰ Scheduled prompt {} failed: {failure}", item.label());
            make_ready(connection, id, &result, &failure, now)?;
            Ok(true)
        })
    }

    /// Claims up to `limit` deliverable items for `scope`: `ready` ones, and
    /// due `waiting` text items, which become `ready` with result `⏰ <body>`.
    pub fn claim_deliveries(
        &self,
        scope: &Scope,
        lease: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Item>> {
        self.write(|connection| {
            let candidates = query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE next_try_at <= ?1 AND claimed_until <= ?1 \
                     AND (state = 'ready' OR (state = 'waiting' AND kind = 'text')) \
                     ORDER BY next_try_at, id LIMIT 200"
                ),
                params![ms(now)],
            )?;
            let claimed_until = now + TimeDelta::from_std(lease).unwrap_or(TimeDelta::minutes(3));
            let mut claimed = Vec::new();
            for mut item in candidates {
                if claimed.len() >= limit {
                    break;
                }
                if !scope.matches(&item.origin) {
                    continue;
                }
                if item.state == State::Waiting {
                    item.result = format!("⏰ {}", item.body);
                    item.state = State::Ready;
                }
                item.claimed_until = claimed_until;
                connection.execute(
                    "UPDATE items SET state = 'ready', result = ?2, claimed_until = ?3 WHERE id = ?1",
                    params![item.id, item.result, ms(claimed_until)],
                )?;
                claimed.push(item);
            }
            Ok(claimed)
        })
    }

    /// Records the delivery of occurrence `(id, due_at)` and advances a
    /// recurring item or sets it `done`. `false` when the occurrence is no
    /// longer current.
    pub fn delivered(&self, id: i64, due_at: DateTime<Utc>, now: DateTime<Utc>) -> Result<bool> {
        self.write(|connection| {
            let Some(item) = current(connection, id, due_at, State::Ready)? else {
                return Ok(false);
            };
            connection.execute(
                "INSERT INTO deliveries (item_id, due_at, origin, session_id, text, delivered_at, appended) \
                 VALUES (?1,?2,?3,?4,?5,?6,0) ON CONFLICT(item_id, due_at) DO NOTHING",
                params![
                    id,
                    ms(due_at),
                    item.origin.as_str(),
                    item.session_id,
                    item.result,
                    ms(now)
                ],
            )?;
            if item.recurrence().is_some() {
                advance(connection, &item, now, "")?;
            } else {
                connection.execute(
                    "UPDATE items SET state = 'done', last_error = '', claimed_until = 0, \
                     updated_at = ?2 WHERE id = ?1",
                    params![id, ms(now)],
                )?;
            }
            Ok(true)
        })
    }

    /// `attempts + 1`, `next_try_at = now + backoff`, the claim released.
    pub fn delivery_failed(
        &self,
        id: i64,
        due_at: DateTime<Utc>,
        error: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.write(|connection| {
            let Some(item) = current(connection, id, due_at, State::Ready)? else {
                return Ok(());
            };
            let attempts = item.attempts + 1;
            connection.execute(
                "UPDATE items SET attempts = ?2, next_try_at = ?3, last_error = ?4, \
                 claimed_until = 0, updated_at = ?5 WHERE id = ?1",
                params![
                    id,
                    attempts,
                    ms(now + backoff(attempts)),
                    cut(error, MAX_ERROR_CHARS),
                    ms(now)
                ],
            )?;
            Ok(())
        })
    }

    /// The earliest instant at which an active row becomes claimable, for
    /// worker sleeps.
    pub fn next_due(&self, _now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        self.read(|connection| {
            let earliest: Option<i64> = connection.query_row(
                "SELECT MIN(MAX(next_try_at, claimed_until)) FROM items \
                 WHERE state IN ('waiting','ready')",
                (),
                |row| row.get(0),
            )?;
            Ok(earliest.map(from_ms))
        })
    }

    /// Up to `limit` deliveries not yet written back, oldest first. For a chat
    /// origin: rows of that origin. For terminal and web: rows of
    /// `session_id` whose origin is not a chat.
    pub fn pending_writebacks(
        &self,
        origin: &Origin,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Delivery>> {
        self.read(|connection| {
            let (filter, key) = if origin.is_chat() {
                ("origin = ?1", origin.as_str())
            } else {
                ("session_id = ?1 AND origin NOT LIKE 'chat:%'", session_id)
            };
            let mut statement = connection.prepare(&format!(
                "SELECT item_id, due_at, origin, session_id, text, delivered_at FROM deliveries \
                 WHERE appended = 0 AND {filter} ORDER BY delivered_at, item_id, due_at LIMIT ?2"
            ))?;
            statement
                .query_map(params![key, limit as i64], |row| {
                    let origin: String = row.get(2)?;
                    Ok(Delivery {
                        item_id: row.get(0)?,
                        due_at: from_ms(row.get(1)?),
                        origin: Origin::parse(&origin).map_err(|_| {
                            crate::storage::Error::ConversionFailure("bad origin".into())
                        })?,
                        session_id: row.get(3)?,
                        text: row.get(4)?,
                        delivered_at: from_ms(row.get(5)?),
                    })
                })?
                .collect()
        })
    }

    pub fn mark_appended(&self, item_id: i64, due_at: DateTime<Utc>) -> Result<()> {
        self.write(|connection| {
            connection.execute(
                "UPDATE deliveries SET appended = 1 WHERE item_id = ?1 AND due_at = ?2",
                params![item_id, ms(due_at)],
            )?;
            Ok(())
        })
    }
}

fn query_items(
    connection: &Connection,
    sql: &str,
    params: impl crate::storage::IntoParams,
) -> Db<Vec<Item>> {
    connection
        .prepare(sql)?
        .query_map(params, Item::from_sql)?
        .collect()
}

fn get(connection: &Connection, id: i64) -> Db<Option<Item>> {
    Ok(query_items(
        connection,
        &format!("SELECT {COLUMNS} FROM items WHERE id = ?1"),
        params![id],
    )?
    .pop())
}

/// The item when `(id, due_at)` is still its current occurrence and it is in `state`.
fn current(
    connection: &Connection,
    id: i64,
    due_at: DateTime<Utc>,
    state: State,
) -> Db<Option<Item>> {
    Ok(get(connection, id)?.filter(|item| item.due_at == due_at && item.state == state))
}

fn make_ready(
    connection: &Connection,
    id: i64,
    result: &str,
    error: &str,
    now: DateTime<Utc>,
) -> Db<()> {
    connection.execute(
        "UPDATE items SET state = 'ready', result = ?2, last_error = ?3, attempts = 0, \
         next_try_at = ?4, claimed_until = 0, updated_at = ?4 WHERE id = ?1",
        params![id, result, error, ms(now)],
    )?;
    Ok(())
}

/// Moves a recurring item to its next occurrence after `now`.
fn advance(connection: &Connection, item: &Item, now: DateTime<Utc>, error: &str) -> Db<()> {
    let Some((repeat, time)) = item.recurrence() else {
        return Ok(());
    };
    let due = next_occurrence(repeat, time, now, &Local);
    connection.execute(
        "UPDATE items SET due_at = ?2, next_try_at = ?2, state = 'waiting', result = '', \
         attempts = 0, claimed_until = 0, last_error = ?3, updated_at = ?4 WHERE id = ?1",
        params![item.id, ms(due), error, ms(now)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn local(text: &str) -> DateTime<Utc> {
        let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    /// The wall clock at the store's millisecond resolution.
    fn now_ms() -> DateTime<Utc> {
        from_ms(ms(Utc::now()))
    }

    fn origin() -> Origin {
        Origin::chat("telegram", "42").unwrap()
    }

    fn new_item(kind: Kind, body: &str, due_at: DateTime<Utc>) -> NewItem {
        NewItem {
            origin: origin(),
            session_id: "sess-1".into(),
            workspace: "/work".into(),
            kind,
            body: body.into(),
            repeat: None,
            local_time: None,
            due_at,
        }
    }

    fn recurring(repeat: Repeat, time: &str, due_at: DateTime<Utc>) -> NewItem {
        NewItem {
            repeat: Some(repeat),
            local_time: Some(NaiveTime::parse_from_str(time, "%H:%M").unwrap()),
            ..new_item(Kind::Text, "daily", due_at)
        }
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn item(store: &Store, id: i64) -> Item {
        store
            .read(|connection| get(connection, id))
            .unwrap()
            .unwrap()
    }

    fn count(store: &Store, table: &str) -> i64 {
        store
            .read(|connection| {
                connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), (), |row| {
                    row.get(0)
                })
            })
            .unwrap()
    }

    const ANY_LEASE: Duration = Duration::from_secs(180);

    #[test]
    fn create_limits_active_items_per_origin() {
        let store = store();
        let now = now_ms();
        for _ in 0..32 {
            store.create(new_item(Kind::Text, "x", now), now).unwrap();
        }
        assert_eq!(
            store
                .create(new_item(Kind::Text, "x", now), now)
                .unwrap_err(),
            CreateError::Limit
        );
        let other = NewItem {
            origin: Origin::web(),
            ..new_item(Kind::Text, "x", now)
        };
        store.create(other, now).unwrap();
        store.cancel(&origin(), 1, now).unwrap().unwrap();
        store.create(new_item(Kind::Text, "x", now), now).unwrap();
    }

    #[test]
    fn a_delivery_claim_holds_until_its_lease_expires() {
        let store = store();
        let now = now_ms();
        store.create(new_item(Kind::Text, "hi", now), now).unwrap();
        let first = store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].result, "⏰ hi");
        assert!(
            store
                .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
                .unwrap()
                .is_empty()
        );
        let later = now + TimeDelta::seconds(181);
        assert_eq!(
            store
                .claim_deliveries(&Scope::Any, ANY_LEASE, 10, later)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn an_item_is_not_claimable_before_it_is_due() {
        let store = store();
        let now = now_ms();
        store
            .create(
                new_item(Kind::Text, "later", now + TimeDelta::seconds(60)),
                now,
            )
            .unwrap();
        assert!(
            store
                .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
                .unwrap()
                .is_empty()
        );
        let due = now + TimeDelta::seconds(60);
        assert_eq!(
            store
                .claim_deliveries(&Scope::Any, ANY_LEASE, 10, due)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn delivery_completes_a_one_shot_item_once() {
        let store = store();
        let now = now_ms();
        let created = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
            .unwrap();
        assert!(store.delivered(created.id, created.due_at, now).unwrap());
        assert!(!store.delivered(created.id, created.due_at, now).unwrap());
        assert_eq!(item(&store, created.id).state, State::Done);
        assert_eq!(count(&store, "deliveries"), 1);
        let pending = store
            .pending_writebacks(&origin(), "other-session", 10)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "⏰ hi");
        store.mark_appended(created.id, created.due_at).unwrap();
        assert!(
            store
                .pending_writebacks(&origin(), "other-session", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cancelled_items_ignore_late_results() {
        let store = store();
        let now = now_ms();
        let prompt = store
            .create(new_item(Kind::Prompt, "check", now), now)
            .unwrap();
        store.claim_run(&Scope::Any, None, now).unwrap().unwrap();
        store.cancel(&origin(), prompt.id, now).unwrap().unwrap();
        let outcome = RunOutcome::Reply("late".into());
        assert!(
            !store
                .finish_run(prompt.id, prompt.due_at, outcome, now)
                .unwrap()
        );
        assert_eq!(item(&store, prompt.id).state, State::Cancelled);

        let text = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
            .unwrap();
        store.cancel(&origin(), text.id, now).unwrap().unwrap();
        assert!(!store.delivered(text.id, text.due_at, now).unwrap());
        store
            .delivery_failed(text.id, text.due_at, "e", now)
            .unwrap();
        let after = item(&store, text.id);
        assert_eq!((after.state, after.attempts), (State::Cancelled, 0));
        assert_eq!(count(&store, "deliveries"), 0);
        assert!(
            store.cancel(&origin(), text.id, now).unwrap().is_none(),
            "already cancelled"
        );
        assert!(
            store
                .cancel(&Origin::web(), prompt.id, now)
                .unwrap()
                .is_none(),
            "other origin"
        );
    }

    #[test]
    fn results_for_an_old_occurrence_are_ignored() {
        let store = store();
        let now = now_ms();
        let created = store
            .create(new_item(Kind::Prompt, "check", now), now)
            .unwrap();
        store.claim_run(&Scope::Any, None, now).unwrap().unwrap();
        let stale = now - TimeDelta::seconds(5);
        assert!(
            !store
                .finish_run(created.id, stale, RunOutcome::Reply("x".into()), now)
                .unwrap()
        );
        assert_eq!(item(&store, created.id).state, State::Waiting);
    }

    #[test]
    fn run_claims_respect_scope_workspace_kind_and_lease() {
        let store = store();
        let now = now_ms();
        store
            .create(new_item(Kind::Text, "text", now), now)
            .unwrap();
        assert!(
            store.claim_run(&Scope::Any, None, now).unwrap().is_none(),
            "text is not run"
        );
        let prompt = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
        assert!(store.claim_run(&Scope::Web, None, now).unwrap().is_none());
        assert!(
            store
                .claim_run(&Scope::Any, Some("/other"), now)
                .unwrap()
                .is_none()
        );
        let chats = Scope::Chats(vec!["telegram".into()]);
        let claimed = store
            .claim_run(&chats, Some("/work"), now)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, prompt.id);
        assert!(
            store.claim_run(&Scope::Any, None, now).unwrap().is_none(),
            "leased"
        );
        assert!(
            store
                .claim_run(&Scope::Any, None, now + TimeDelta::minutes(14))
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .claim_run(&Scope::Any, None, now + TimeDelta::minutes(16))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn run_results_become_delivery_text() {
        let store = store();
        let now = now_ms();
        let cases = [
            (RunOutcome::Reply("  sunny \n".into()), "⏰ sunny"),
            (
                RunOutcome::Reply("  ".into()),
                "⏰ Scheduled prompt s1 finished with no reply.",
            ),
            (
                RunOutcome::Failed("boom".into()),
                "⏰ Scheduled prompt s1 failed: boom",
            ),
        ];
        for (outcome, expected) in cases {
            let store = &store;
            store.write(|c| c.execute("DELETE FROM items", ())).unwrap();
            // ids keep counting; compute the label from the created item.
            let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
            store.claim_run(&Scope::Any, None, now).unwrap().unwrap();
            assert!(
                store
                    .finish_run(created.id, created.due_at, outcome, now)
                    .unwrap()
            );
            let done = item(store, created.id);
            assert_eq!(done.state, State::Ready);
            assert_eq!(done.result, expected.replace("s1", &created.label()));
            assert_eq!(done.claimed_until, DateTime::<Utc>::default());
        }
    }

    #[test]
    fn a_failure_before_tools_is_retried_with_backoff_until_24_hours() {
        let store = store();
        let now = now_ms();
        let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
        let mut at = now;
        for expected in [30, 60, 120, 240, 480, 960, 1800, 1800] {
            store.claim_run(&Scope::Any, None, at).unwrap().unwrap();
            let outcome = RunOutcome::FailedBeforeTools("provider down".into());
            assert!(
                store
                    .finish_run(created.id, created.due_at, outcome, at)
                    .unwrap()
            );
            let row = item(&store, created.id);
            assert_eq!(row.state, State::Waiting);
            assert_eq!(row.last_error, "provider down");
            assert_eq!(row.next_try_at - at, TimeDelta::seconds(expected));
            assert!(store.claim_run(&Scope::Any, None, at).unwrap().is_none());
            at = row.next_try_at;
        }
        let late = now + TimeDelta::hours(24);
        store.claim_run(&Scope::Any, None, late).unwrap().unwrap();
        let outcome = RunOutcome::FailedBeforeTools("provider down".into());
        store
            .finish_run(created.id, created.due_at, outcome, late)
            .unwrap();
        let row = item(&store, created.id);
        assert_eq!(row.state, State::Ready);
        assert_eq!(row.result, "⏰ Scheduled prompt s1 failed: provider down");
    }

    #[test]
    fn a_failure_text_is_cut_to_300_characters() {
        let store = store();
        let now = now_ms();
        let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
        store.claim_run(&Scope::Any, None, now).unwrap();
        let outcome = RunOutcome::Failed("e".repeat(1000));
        store
            .finish_run(created.id, created.due_at, outcome, now)
            .unwrap();
        let row = item(&store, created.id);
        assert_eq!(
            row.result.chars().count(),
            "⏰ Scheduled prompt s1 failed: ".chars().count() + 303
        );
    }

    #[test]
    fn delivery_failures_back_off_to_30_minutes_and_clear_on_success() {
        let store = store();
        let now = now_ms();
        let created = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
        let mut at = now;
        for expected in [30, 60, 120, 240, 480, 960, 1800, 1800] {
            let claimed = store
                .claim_deliveries(&Scope::Any, ANY_LEASE, 1, at)
                .unwrap();
            assert_eq!(claimed.len(), 1, "claimable at {at}");
            store
                .delivery_failed(created.id, created.due_at, "send failed", at)
                .unwrap();
            let row = item(&store, created.id);
            assert_eq!(row.last_error, "send failed");
            assert_eq!(row.next_try_at - at, TimeDelta::seconds(expected));
            assert!(
                store
                    .claim_deliveries(&Scope::Any, ANY_LEASE, 1, at)
                    .unwrap()
                    .is_empty()
            );
            at = row.next_try_at;
        }
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 1, at)
            .unwrap();
        store.delivered(created.id, created.due_at, at).unwrap();
        assert_eq!(item(&store, created.id).last_error, "");
    }

    #[test]
    fn claim_deliveries_honours_scope_and_limit() {
        let store = store();
        let now = now_ms();
        for _ in 0..3 {
            store
                .create(new_item(Kind::Text, "chat", now), now)
                .unwrap();
        }
        let web = NewItem {
            origin: Origin::web(),
            ..new_item(Kind::Text, "web", now)
        };
        store.create(web, now).unwrap();
        let claimed = store
            .claim_deliveries(&Scope::Web, ANY_LEASE, 10, now)
            .unwrap();
        assert_eq!(
            claimed.iter().map(|i| i.body.as_str()).collect::<Vec<_>>(),
            ["web"]
        );
        let chats = Scope::Chats(vec!["telegram".into()]);
        assert_eq!(
            store
                .claim_deliveries(&chats, ANY_LEASE, 2, now)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_one_shot_occurrence_fails_after_24_hours() {
        let store = store();
        let now = now_ms();
        let created = store.create(new_item(Kind::Text, "old", now), now).unwrap();
        store.sweep(now + TimeDelta::hours(23)).unwrap();
        assert_eq!(item(&store, created.id).state, State::Waiting);
        store.sweep(now + TimeDelta::hours(25)).unwrap();
        let row = item(&store, created.id);
        assert_eq!(row.state, State::Failed);
        assert_eq!(row.last_error, EXPIRED);
    }

    #[test]
    fn sweep_leaves_claimed_rows_alone() {
        let store = store();
        let now = now_ms();
        let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
        let claim_time = now + TimeDelta::hours(24);
        store
            .claim_run(&Scope::Any, None, claim_time)
            .unwrap()
            .unwrap();
        store.sweep(claim_time + TimeDelta::minutes(1)).unwrap();
        assert_eq!(item(&store, created.id).state, State::Waiting);
        store.sweep(claim_time + TimeDelta::minutes(16)).unwrap();
        assert_eq!(item(&store, created.id).state, State::Failed);
    }

    #[test]
    fn a_missed_recurring_occurrence_catches_up_within_24_hours() {
        let store = store();
        let due = local("2026-10-02 08:00");
        let now = local("2026-10-04 12:00");
        let created = store
            .create(recurring(Repeat::Daily, "08:00", due), due)
            .unwrap();
        store.sweep(now).unwrap();
        let row = item(&store, created.id);
        assert_eq!(row.state, State::Waiting);
        assert_eq!(row.due_at, local("2026-10-04 08:00"));
        assert_eq!(row.next_try_at, row.due_at);
        assert_eq!(row.last_error, "");
    }

    #[test]
    fn a_recurring_occurrence_older_than_24_hours_is_skipped() {
        let store = store();
        // 2026-10-02 is a Friday; on Sunday noon the latest weekday occurrence is 52 h old.
        let due = local("2026-10-02 08:00");
        let now = local("2026-10-04 12:00");
        let created = store
            .create(recurring(Repeat::Weekdays, "08:00", due), due)
            .unwrap();
        store.sweep(now).unwrap();
        let row = item(&store, created.id);
        assert_eq!(row.state, State::Waiting);
        assert_eq!(row.due_at, local("2026-10-05 08:00"));
        assert_eq!(row.last_error, EXPIRED);
    }

    #[test]
    fn an_undelivered_ready_recurring_occurrence_advances_after_24_hours() {
        let store = store();
        let due = local("2026-10-01 08:00");
        let created = store
            .create(recurring(Repeat::Daily, "08:00", due), due)
            .unwrap();
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 1, due)
            .unwrap();
        let now = local("2026-10-02 09:00");
        store.sweep(now).unwrap();
        let row = item(&store, created.id);
        assert_eq!(
            (row.state, row.last_error.as_str()),
            (State::Waiting, EXPIRED)
        );
        assert_eq!(row.due_at, local("2026-10-03 08:00"));
        assert_eq!(row.result, "");
    }

    #[test]
    fn delivering_a_recurring_item_advances_it() {
        let store = store();
        let due = local("2026-10-01 08:00");
        let created = store
            .create(recurring(Repeat::Daily, "08:00", due), due)
            .unwrap();
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 1, due)
            .unwrap();
        store.delivery_failed(created.id, due, "x", due).unwrap();
        let retry = due + TimeDelta::seconds(30);
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 1, retry)
            .unwrap();
        assert!(store.delivered(created.id, due, retry).unwrap());
        let row = item(&store, created.id);
        assert_eq!((row.state, row.attempts), (State::Waiting, 0));
        assert_eq!(row.due_at, local("2026-10-02 08:00"));
        assert_eq!((row.last_error.as_str(), row.result.as_str()), ("", ""));
    }

    #[test]
    fn sweep_deletes_ended_rows_and_deliveries_after_7_days() {
        let store = store();
        let now = now_ms();
        let created = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 1, now)
            .unwrap();
        store.delivered(created.id, created.due_at, now).unwrap();
        store.sweep(now + TimeDelta::days(6)).unwrap();
        assert_eq!(
            (count(&store, "items"), count(&store, "deliveries")),
            (1, 1)
        );
        store.sweep(now + TimeDelta::days(8)).unwrap();
        assert_eq!(
            (count(&store, "items"), count(&store, "deliveries")),
            (0, 0)
        );
    }

    #[test]
    fn list_shows_active_then_recently_ended_items() {
        let store = store();
        let now = now_ms();
        let ended = store
            .create(new_item(Kind::Text, "ended", now), now)
            .unwrap();
        store.cancel(&origin(), ended.id, now).unwrap();
        store
            .create(
                new_item(Kind::Text, "active", now + TimeDelta::hours(1)),
                now,
            )
            .unwrap();
        store
            .create(
                NewItem {
                    origin: Origin::web(),
                    ..new_item(Kind::Text, "other", now)
                },
                now,
            )
            .unwrap();
        let bodies = |at| {
            store
                .list(&origin(), at)
                .unwrap()
                .into_iter()
                .map(|i| i.body)
                .collect::<Vec<_>>()
        };
        assert_eq!(bodies(now), ["active", "ended"]);
        assert_eq!(bodies(now + TimeDelta::days(8)), ["active"]);
    }

    #[test]
    fn writebacks_match_chat_by_origin_and_terminal_by_session() {
        let store = store();
        let now = now_ms();
        let mut ids = Vec::new();
        let specs = [
            ("chat-a", origin(), "sess-1"),
            ("chat-b", origin(), "sess-2"),
            (
                "other-chat",
                Origin::chat("telegram", "7").unwrap(),
                "sess-1",
            ),
            ("term-1", Origin::terminal(), "sess-1"),
            ("web-1", Origin::web(), "sess-1"),
            ("term-2", Origin::terminal(), "sess-2"),
        ];
        for (body, origin, session) in specs {
            let new = NewItem {
                origin,
                session_id: session.into(),
                ..new_item(Kind::Text, body, now)
            };
            let created = store.create(new, now).unwrap();
            ids.push((created.id, created.due_at));
        }
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
            .unwrap();
        for (offset, (id, due)) in ids.iter().enumerate() {
            store
                .delivered(*id, *due, now + TimeDelta::seconds(offset as i64))
                .unwrap();
        }
        let texts = |origin: &Origin, session: &str, limit| {
            store
                .pending_writebacks(origin, session, limit)
                .unwrap()
                .into_iter()
                .map(|d| d.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&origin(), "sess-9", 10), ["⏰ chat-a", "⏰ chat-b"]);
        assert_eq!(texts(&origin(), "sess-9", 1), ["⏰ chat-a"]);
        assert_eq!(
            texts(&Origin::terminal(), "sess-1", 10),
            ["⏰ term-1", "⏰ web-1"]
        );
        assert_eq!(texts(&Origin::web(), "sess-2", 10), ["⏰ term-2"]);
    }

    #[test]
    fn next_due_is_the_earliest_claimable_instant() {
        let store = store();
        let now = now_ms();
        assert_eq!(store.next_due(now).unwrap(), None);
        store
            .create(
                new_item(Kind::Text, "later", now + TimeDelta::seconds(90)),
                now,
            )
            .unwrap();
        store
            .create(
                new_item(Kind::Text, "soon", now + TimeDelta::seconds(30)),
                now,
            )
            .unwrap();
        let soon = (now + TimeDelta::seconds(30)).timestamp_millis();
        assert_eq!(
            store.next_due(now).unwrap().unwrap().timestamp_millis(),
            soon
        );
    }

    #[test]
    fn delivery_text_adds_a_late_line_after_two_minutes() {
        let store = store();
        let now = now_ms();
        store.create(new_item(Kind::Text, "hi", now), now).unwrap();
        let claimed = |at| {
            store
                .claim_deliveries(&Scope::Any, Duration::ZERO, 1, at)
                .unwrap()
                .remove(0)
        };
        let on_time = claimed(now + TimeDelta::seconds(120));
        assert_eq!(
            delivery_text(&on_time, now + TimeDelta::seconds(120)),
            "⏰ hi"
        );
        let at = now + TimeDelta::seconds(2 * 3600 + 3 * 60);
        let late = claimed(at);
        let expected = format!(
            "⏰ hi\n(scheduled for {}, delivered 2h3m late)",
            format_local(now)
        );
        assert_eq!(delivery_text(&late, at), expected);
    }

    #[test]
    fn run_and_context_text_carry_the_schedule_facts() {
        let now = now_ms();
        let created = store()
            .create(new_item(Kind::Prompt, "check rain", now), now)
            .unwrap();
        let text = run_text(&created, now);
        assert!(
            text.starts_with(&format!("[scheduled run s{}: created ", created.id)),
            "{text}"
        );
        assert!(text.contains("from a telegram chat;"), "{text}");
        assert!(
            text.ends_with("No user is present; approval requests are denied.]\n\ncheck rain"),
            "{text}"
        );
        let delivery = Delivery {
            item_id: 7,
            due_at: now,
            origin: origin(),
            session_id: "s".into(),
            text: "⏰ x".into(),
            delivered_at: now,
        };
        assert_eq!(
            delivery.context_text(),
            format!("[scheduled s7, due {}]\n⏰ x", format_local(now))
        );
    }

    #[test]
    fn the_file_is_private_and_an_unknown_schema_version_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sub").join("schedule.db");
        drop(Store::open(&path).unwrap());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let raw = Connection::open(&path).unwrap();
        raw.pragma_update("user_version", 99).unwrap();
        drop(raw);
        assert!(Store::open(&path).is_err());
    }
}
