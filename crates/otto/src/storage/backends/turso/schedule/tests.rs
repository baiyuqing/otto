use crate::schedule::time::format_local;
use crate::schedule::{delivery_text, run_text};
use chrono::TimeZone;
use std::sync::Arc;

use super::*;

mod claims_and_backoff;
mod delivery_and_recurrence;

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
