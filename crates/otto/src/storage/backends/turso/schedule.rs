//! Turso implementation of the schedule repository port.
use crate::schedule::time::{latest_occurrence, next_occurrence};
use crate::schedule::{
    CreateError, Delivery, Error, Item, Kind, NewItem, Origin, Repeat, Result, RunOutcome,
    ScheduleStore, Scope, State,
};
use crate::storage::drivers::turso::TursoDriver;
use crate::storage::{Connection, params};
use crate::storage::{OpenOptions, SqlDriver, SqlRequirements};
use chrono::{DateTime, Local, NaiveTime, TimeDelta, Utc};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;

pub(super) const SCHEMA_VERSION: i64 = 2;

pub(super) const SCHEMA: &str = r#"
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
CREATE TABLE IF NOT EXISTS delivery_clients (
    id TEXT PRIMARY KEY,
    platforms TEXT NOT NULL,
    destinations TEXT,
    expires_at INTEGER NOT NULL
) STRICT;
"#;

const COLUMNS: &str = "id, origin, session_id, workspace, kind, body, repeat, local_time, due_at, \
state, result, attempts, next_try_at, claimed_until, last_error, created_at, updated_at";

/// Active items (`waiting` or `ready`) one origin may hold.
const MAX_ACTIVE_PER_ORIGIN: i64 = 32;
const RUN_LEASE: Duration = Duration::from_secs(15 * 60);
/// An occurrence not delivered this long after its due time is failed.
const EXPIRY: TimeDelta = TimeDelta::hours(24);
const RETENTION: TimeDelta = TimeDelta::days(7);
const BACKOFF_BASE_SECONDS: i64 = 30;
const BACKOFF_CAP_SECONDS: i64 = 30 * 60;
const MAX_ERROR_CHARS: usize = 300;
const EXPIRED: &str = "not delivered within 24 h of the due time";

type Db<T> = crate::storage::Result<T>;
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
        format!("{}...", text.chars().take(max).collect::<String>())
    }
}
fn backoff(attempts: i64) -> TimeDelta {
    TimeDelta::seconds(
        (BACKOFF_BASE_SECONDS << (attempts - 1).clamp(0, 16) as u32).min(BACKOFF_CAP_SECONDS),
    )
}
fn recurrence(item: &Item) -> Option<(Repeat, NaiveTime)> {
    Some((item.repeat?, item.local_time?))
}
impl Item {
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

pub struct TursoScheduleStore {
    connection: Mutex<Connection>,
    notify: Notify,
}
#[cfg(test)]
type Store = TursoScheduleStore;
impl TursoScheduleStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_driver(&TursoDriver, path)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error)?;
        }
        let connection = Connection::open_with_driver(
            driver,
            path,
            OpenOptions::default(),
            SqlRequirements::DURABLE,
        )
        .map_err(|_| Error)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error)?;
        Self::from_connection(connection)
    }

    pub(crate) fn from_connection(connection: Connection) -> Result<Self> {
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
        initialize_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            notify: Notify::new(),
        })
    }

    /// A live connection refreshes this lease independently of slow deliveries.
    pub fn register_delivery_client(
        &self,
        id: &str,
        platforms: &[String],
        destinations: Option<&[Origin]>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let platforms = serde_json::to_string(platforms).map_err(|_| Error)?;
        let destinations = destinations
            .map(|origins| {
                serde_json::to_string(&origins.iter().map(Origin::as_str).collect::<Vec<_>>())
            })
            .transpose()
            .map_err(|_| Error)?;
        self.write(|connection| {
            connection.execute("DELETE FROM delivery_clients WHERE expires_at < ?1", params![ms(now - RETENTION)])?;
            connection.execute("INSERT INTO delivery_clients (id, platforms, destinations, expires_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, platforms, destinations, ms(now + TimeDelta::seconds(90))])?;
            Ok(())
        })
    }

    pub fn refresh_delivery_client(&self, id: &str, now: DateTime<Utc>) -> Result<Vec<String>> {
        self.write(|connection| {
            connection.execute(
                "UPDATE delivery_clients SET expires_at = ?2 WHERE id = ?1",
                params![id, ms(now + TimeDelta::seconds(90))],
            )?;
            delivery_scope(connection, id, now).map(|(_, conflicts)| conflicts)
        })
    }

    pub fn unregister_delivery_client(&self, id: &str) -> Result<()> {
        self.write(|connection| {
            connection.execute("DELETE FROM delivery_clients WHERE id = ?1", params![id])?;
            Ok(())
        })?;
        self.notify.notify_waiters();
        Ok(())
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
                match recurrence(&item) {
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
            let resolved;
            let scope = if let Scope::DeliveryClient(id) = scope {
                resolved = delivery_scope(connection, id, now)?.0;
                &resolved
            } else { scope };
            // ponytail: scan due rows for dynamic scopes; use SQL filters if queues grow.

            let candidates = query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE next_try_at <= ?1 AND claimed_until <= ?1 \
                     AND (state = 'ready' OR (state = 'waiting' AND kind = 'text')) \
                     ORDER BY next_try_at, id"
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
            if recurrence(&item).is_some() {
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

pub(super) fn initialize_schema(connection: &Connection) -> Result<()> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", (), |row| row.get(0))
        .map_err(|_| Error)?;
    match version {
        0 | 1 => {
            let transaction = connection.unchecked_transaction().map_err(|_| Error)?;
            transaction.execute_batch(SCHEMA).map_err(|_| Error)?;
            transaction
                .pragma_update("user_version", SCHEMA_VERSION)
                .map_err(|_| Error)?;
            transaction.commit().map_err(|_| Error)?;
        }
        version if version == SCHEMA_VERSION => {}
        _ => return Err(Error),
    }
    Ok(())
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

/// Exact clients keep their destinations. A legacy platform is eligible only
/// while this is the sole live connection offering that platform.
fn delivery_scope(
    connection: &Connection,
    id: &str,
    now: DateTime<Utc>,
) -> Db<(Scope, Vec<String>)> {
    let mut statement = connection.prepare(
        "SELECT id, platforms, destinations FROM delivery_clients WHERE expires_at > ?1",
    )?;
    let rows = statement
        .query_map(params![ms(now)], |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<Option<String>>(2)?,
            ))
        })?
        .collect::<Db<Vec<_>>>()?;
    let bad = || crate::storage::Error::ConversionFailure("bad delivery client".into());
    let mut clients = Vec::new();
    for (client, platforms, destinations) in rows {
        let platforms: Vec<String> = serde_json::from_str(&platforms).map_err(|_| bad())?;
        clients.push((client, platforms, destinations));
    }
    let Some((_, platforms, destinations)) = clients.iter().find(|(client, _, _)| client == id)
    else {
        return Ok((Scope::Origins(Vec::new()), Vec::new()));
    };
    if let Some(destinations) = destinations {
        let values: Vec<String> = serde_json::from_str(destinations).map_err(|_| bad())?;
        let origins = values
            .iter()
            .map(|value| Origin::parse(value).map_err(|_| bad()))
            .collect::<Db<Vec<_>>>()?;
        return Ok((Scope::Origins(origins), Vec::new()));
    }
    let (allowed, conflicts): (Vec<_>, Vec<_>) = platforms.iter().cloned().partition(|platform| {
        clients
            .iter()
            .filter(|(_, offers, _)| offers.contains(platform))
            .count()
            == 1
    });
    Ok((Scope::Chats(allowed), conflicts))
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
    let Some((repeat, time)) = recurrence(item) else {
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

impl ScheduleStore for TursoScheduleStore {
    fn register_delivery_client(
        &self,
        id: &str,
        p: &[String],
        d: Option<&[Origin]>,
        n: DateTime<Utc>,
    ) -> Result<()> {
        TursoScheduleStore::register_delivery_client(self, id, p, d, n)
    }
    fn refresh_delivery_client(&self, id: &str, n: DateTime<Utc>) -> Result<Vec<String>> {
        TursoScheduleStore::refresh_delivery_client(self, id, n)
    }
    fn unregister_delivery_client(&self, id: &str) -> Result<()> {
        TursoScheduleStore::unregister_delivery_client(self, id)
    }
    fn notifier(&self) -> &Notify {
        TursoScheduleStore::notifier(self)
    }
    fn create(&self, i: NewItem, n: DateTime<Utc>) -> std::result::Result<Item, CreateError> {
        TursoScheduleStore::create(self, i, n)
    }
    fn list(&self, o: &Origin, n: DateTime<Utc>) -> Result<Vec<Item>> {
        TursoScheduleStore::list(self, o, n)
    }
    fn cancel(&self, o: &Origin, id: i64, n: DateTime<Utc>) -> Result<Option<Item>> {
        TursoScheduleStore::cancel(self, o, id, n)
    }
    fn sweep(&self, n: DateTime<Utc>) -> Result<()> {
        TursoScheduleStore::sweep(self, n)
    }
    fn claim_run(&self, s: &Scope, w: Option<&str>, n: DateTime<Utc>) -> Result<Option<Item>> {
        TursoScheduleStore::claim_run(self, s, w, n)
    }
    fn finish_run(
        &self,
        id: i64,
        d: DateTime<Utc>,
        o: RunOutcome,
        n: DateTime<Utc>,
    ) -> Result<bool> {
        TursoScheduleStore::finish_run(self, id, d, o, n)
    }
    fn claim_deliveries(
        &self,
        s: &Scope,
        l: Duration,
        c: usize,
        n: DateTime<Utc>,
    ) -> Result<Vec<Item>> {
        TursoScheduleStore::claim_deliveries(self, s, l, c, n)
    }
    fn delivered(&self, id: i64, d: DateTime<Utc>, n: DateTime<Utc>) -> Result<bool> {
        TursoScheduleStore::delivered(self, id, d, n)
    }
    fn delivery_failed(&self, id: i64, d: DateTime<Utc>, e: &str, n: DateTime<Utc>) -> Result<()> {
        TursoScheduleStore::delivery_failed(self, id, d, e, n)
    }
    fn next_due(&self, n: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        TursoScheduleStore::next_due(self, n)
    }
    fn pending_writebacks(&self, o: &Origin, s: &str, l: usize) -> Result<Vec<Delivery>> {
        TursoScheduleStore::pending_writebacks(self, o, s, l)
    }
    fn mark_appended(&self, id: i64, d: DateTime<Utc>) -> Result<()> {
        TursoScheduleStore::mark_appended(self, id, d)
    }
}

#[cfg(test)]
mod tests;
