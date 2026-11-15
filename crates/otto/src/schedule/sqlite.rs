//! SQLite implementation of the schedule repository port.
use crate::schedule::time::{latest_occurrence, next_occurrence};
use crate::schedule::{
    CreateError, Delivery, Error, Item, Kind, NewItem, Repeat, Result, RunOutcome, ScheduleStore,
    Scope, State, WorkspaceAdmit,
};
use crate::session::origin::Origin;
use chrono::{DateTime, Local, NaiveTime, TimeDelta, Utc};
use otto_storage::drivers::sqlite::SqliteDriver;
use otto_storage::{Connection, params};
use otto_storage::{OpenOptions, SqlDriver};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;

mod operations;

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
    destinations TEXT NOT NULL,
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

type Db<T> = otto_storage::Result<T>;
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
    fn from_sql(row: &otto_storage::Row) -> Db<Self> {
        let bad = |what: &str| otto_storage::Error::ConversionFailure(format!("bad {what}"));
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

pub struct SqliteScheduleStore {
    connection: Mutex<Connection>,
    notify: Notify,
}
#[cfg(test)]
type Store = SqliteScheduleStore;
impl SqliteScheduleStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_driver(&SqliteDriver, path)
    }

    pub fn open_with_driver(driver: &dyn SqlDriver, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|_| Error)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error)?;
        }
        let connection = Connection::open_with_driver(driver, path, OpenOptions::default())
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
        destinations: &[Origin],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut platforms: Vec<&str> = destinations
            .iter()
            .filter_map(Origin::chat_platform)
            .collect();
        platforms.sort_unstable();
        platforms.dedup();
        let platforms = serde_json::to_string(&platforms).map_err(|_| Error)?;
        let destinations =
            serde_json::to_string(&destinations.iter().map(Origin::as_str).collect::<Vec<_>>())
                .map_err(|_| Error)?;
        self.write(|connection| {
            connection.execute("DELETE FROM delivery_clients WHERE expires_at < ?1", params![ms(now - RETENTION)])?;
            connection.execute("INSERT INTO delivery_clients (id, platforms, destinations, expires_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, platforms, destinations, ms(now + TimeDelta::seconds(90))])?;
            Ok(())
        })
    }

    pub(crate) fn refresh_delivery_client(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        self.write(|connection| {
            connection.execute(
                "UPDATE delivery_clients SET expires_at = ?2 WHERE id = ?1",
                params![id, ms(now + TimeDelta::seconds(90))],
            )?;
            Ok(())
        })
    }

    pub(crate) fn unregister_delivery_client(&self, id: &str) -> Result<()> {
        self.write(|connection| {
            connection.execute("DELETE FROM delivery_clients WHERE id = ?1", params![id])?;
            Ok(())
        })?;
        self.notify.notify_waiters();
        Ok(())
    }

    /// Wakes in-process workers after an item is created or cancelled.
    pub(crate) fn notifier(&self) -> &Notify {
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
}

pub(super) fn initialize_schema(connection: &Connection) -> Result<()> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", (), |row| row.get(0))
        .map_err(|_| Error)?;
    match version {
        0 => {
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
    params: impl otto_storage::IntoParams,
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

/// The destinations a live delivery client registered; none when the client
/// is unknown or expired.
fn delivery_scope(connection: &Connection, id: &str, now: DateTime<Utc>) -> Db<Scope> {
    let destinations = connection
        .prepare("SELECT destinations FROM delivery_clients WHERE id = ?1 AND expires_at > ?2")?
        .query_map(params![id, ms(now)], |row| row.get::<String>(0))?
        .collect::<Db<Vec<_>>>()?
        .pop();
    let bad = || otto_storage::Error::ConversionFailure("bad delivery client".into());
    let Some(destinations) = destinations else {
        return Ok(Scope::Origins(Vec::new()));
    };
    let values: Vec<String> = serde_json::from_str(&destinations).map_err(|_| bad())?;
    let origins = values
        .iter()
        .map(|value| Origin::parse(value).map_err(|_| bad()))
        .collect::<Db<Vec<_>>>()?;
    Ok(Scope::Origins(origins))
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

impl ScheduleStore for SqliteScheduleStore {
    fn register_delivery_client(&self, id: &str, d: &[Origin], n: DateTime<Utc>) -> Result<()> {
        SqliteScheduleStore::register_delivery_client(self, id, d, n)
    }
    fn refresh_delivery_client(&self, id: &str, n: DateTime<Utc>) -> Result<()> {
        SqliteScheduleStore::refresh_delivery_client(self, id, n)
    }
    fn unregister_delivery_client(&self, id: &str) -> Result<()> {
        SqliteScheduleStore::unregister_delivery_client(self, id)
    }
    fn notifier(&self) -> &Notify {
        SqliteScheduleStore::notifier(self)
    }
    fn create(&self, i: NewItem, n: DateTime<Utc>) -> std::result::Result<Item, CreateError> {
        SqliteScheduleStore::create(self, i, n)
    }
    fn list(&self, o: &Origin, n: DateTime<Utc>) -> Result<Vec<Item>> {
        SqliteScheduleStore::list(self, o, n)
    }
    fn cancel(&self, o: &Origin, id: i64, n: DateTime<Utc>) -> Result<Option<Item>> {
        SqliteScheduleStore::cancel(self, o, id, n)
    }
    fn sweep(&self, n: DateTime<Utc>) -> Result<()> {
        SqliteScheduleStore::sweep(self, n)
    }
    fn claim_run(&self, s: &Scope, w: Option<&str>, n: DateTime<Utc>) -> Result<Option<Item>> {
        SqliteScheduleStore::claim_run(self, s, w, n)
    }
    fn claim_run_admitting(
        &self,
        s: &Scope,
        w: Option<&str>,
        a: Option<&WorkspaceAdmit>,
        n: DateTime<Utc>,
    ) -> Result<Option<Item>> {
        SqliteScheduleStore::claim_run_admitting(self, s, w, a, n)
    }
    fn finish_run(
        &self,
        id: i64,
        d: DateTime<Utc>,
        o: RunOutcome,
        n: DateTime<Utc>,
    ) -> Result<bool> {
        SqliteScheduleStore::finish_run(self, id, d, o, n)
    }
    fn claim_deliveries(
        &self,
        s: &Scope,
        l: Duration,
        c: usize,
        n: DateTime<Utc>,
    ) -> Result<Vec<Item>> {
        SqliteScheduleStore::claim_deliveries(self, s, l, c, n)
    }
    fn delivered(&self, id: i64, d: DateTime<Utc>, n: DateTime<Utc>) -> Result<bool> {
        SqliteScheduleStore::delivered(self, id, d, n)
    }
    fn delivery_failed(&self, id: i64, d: DateTime<Utc>, e: &str, n: DateTime<Utc>) -> Result<()> {
        SqliteScheduleStore::delivery_failed(self, id, d, e, n)
    }
    fn next_due(&self, n: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        SqliteScheduleStore::next_due(self, n)
    }
    fn pending_writebacks(&self, o: &Origin, s: &str, l: usize) -> Result<Vec<Delivery>> {
        SqliteScheduleStore::pending_writebacks(self, o, s, l)
    }
    fn mark_appended(&self, id: i64, d: DateTime<Utc>) -> Result<()> {
        SqliteScheduleStore::mark_appended(self, id, d)
    }
}

#[cfg(test)]
mod tests;
