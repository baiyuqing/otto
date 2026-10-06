//! Domain records and the typed schedule persistence port.

use super::{Kind, Origin, Repeat, RunOutcome, Scope, State};
use chrono::{DateTime, NaiveTime, Utc};
use std::fmt;
use tokio::sync::Notify;

/// A storage failure carries no backend details across the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;
impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("schedule store unavailable")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    Limit,
    Store(Error),
}
impl fmt::Display for CreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit => write!(
                formatter,
                "at most 32 scheduled items may be active for this chat or surface; cancel one with schedule_cancel first"
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
impl Item {
    pub fn label(&self) -> String {
        format!("s{}", self.id)
    }
}

/// The row id of a label such as `s7`, the inverse of [`Item::label`].
pub fn parse_label(label: &str) -> Option<i64> {
    label
        .strip_prefix('s')
        .and_then(|digits| digits.parse::<i64>().ok())
        .filter(|number| *number > 0)
}

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
    pub fn context_text(&self) -> String {
        format!(
            "[scheduled s{}, due {}]\n{}",
            self.item_id,
            super::time::format_local(self.due_at),
            self.text
        )
    }
}

/// Typed schedule operations; claims and receipts preserve their existing transaction scope.
pub trait ScheduleStore: Send + Sync {
    fn register_delivery_client(
        &self,
        id: &str,
        platforms: &[String],
        destinations: Option<&[Origin]>,
        now: DateTime<Utc>,
    ) -> Result<()>;
    fn refresh_delivery_client(&self, id: &str, now: DateTime<Utc>) -> Result<Vec<String>>;
    fn unregister_delivery_client(&self, id: &str) -> Result<()>;
    fn notifier(&self) -> &Notify;
    fn create(&self, item: NewItem, now: DateTime<Utc>) -> std::result::Result<Item, CreateError>;
    fn list(&self, origin: &Origin, now: DateTime<Utc>) -> Result<Vec<Item>>;
    fn cancel(&self, origin: &Origin, id: i64, now: DateTime<Utc>) -> Result<Option<Item>>;
    fn sweep(&self, now: DateTime<Utc>) -> Result<()>;
    fn claim_run(
        &self,
        scope: &Scope,
        workspace: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<Item>>;
    fn finish_run(
        &self,
        id: i64,
        due_at: DateTime<Utc>,
        outcome: RunOutcome,
        now: DateTime<Utc>,
    ) -> Result<bool>;
    fn claim_deliveries(
        &self,
        scope: &Scope,
        lease: std::time::Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Item>>;
    fn delivered(&self, id: i64, due_at: DateTime<Utc>, now: DateTime<Utc>) -> Result<bool>;
    fn delivery_failed(
        &self,
        id: i64,
        due_at: DateTime<Utc>,
        error: &str,
        now: DateTime<Utc>,
    ) -> Result<()>;
    fn next_due(&self, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>>;
    fn pending_writebacks(
        &self,
        origin: &Origin,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Delivery>>;
    fn mark_appended(&self, item_id: i64, due_at: DateTime<Utc>) -> Result<()>;
}

pub fn delivery_text(item: &Item, now: DateTime<Utc>) -> String {
    let late = now - item.due_at;
    if late > chrono::TimeDelta::minutes(2) {
        format!(
            "{}\n(scheduled for {}, delivered {} late)",
            item.result,
            super::time::format_local(item.due_at),
            super::time::format_duration(late)
        )
    } else {
        item.result.clone()
    }
}
pub fn run_text(item: &Item, now: DateTime<Utc>) -> String {
    format!(
        "[scheduled run {}: created {} from {}; due {}; now {}. No user is present; approval requests are denied.]\n\n{}",
        item.label(),
        super::time::format_local(item.created_at),
        item.origin.describe(),
        super::time::format_local(item.due_at),
        super::time::format_local(now),
        item.body
    )
}
