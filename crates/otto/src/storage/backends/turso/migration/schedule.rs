//! Schedule occurrences and durable delivery/write-back receipts.
use super::{
    named::{self, Kind, NamedRecord},
    *,
};
use crate::storage::backends::turso::schedule::{self, TursoScheduleStore};

pub struct ScheduleCodec;
const KINDS: &[Kind] = &[
    Kind {
        name: "schedule_occurrence",
        table: "items",
        fields: &[
            "id",
            "origin",
            "session_id",
            "workspace",
            "kind",
            "body",
            "repeat",
            "local_time",
            "due_at",
            "state",
            "result",
            "attempts",
            "next_try_at",
            "claimed_until",
            "last_error",
            "created_at",
            "updated_at",
        ],
        keys: &["id"],
        revision: Some("updated_at"),
    },
    Kind {
        name: "schedule_delivery_receipt",
        table: "deliveries",
        fields: &[
            "item_id",
            "due_at",
            "origin",
            "session_id",
            "text",
            "delivered_at",
            "appended",
        ],
        keys: &["item_id", "due_at"],
        revision: Some("appended"),
    },
    Kind {
        name: "schedule_delivery_client",
        table: "delivery_clients",
        fields: &["id", "platforms", "destinations", "expires_at"],
        keys: &["id"],
        revision: None,
    },
];
fn interrupted(record: &mut NamedRecord) -> Result<()> {
    if record.kind == "schedule_occurrence" {
        let claimed = record.fields["claimed_until"]
            .as_i64()
            .ok_or_else(|| Error::Invalid("invalid schedule execution lease".into()))?;
        if claimed > 0 && record.fields["kind"] == "prompt" && record.fields["state"] == "waiting" {
            record
                .fields
                .insert("state".into(), serde_json::json!("failed"));
            record.fields.insert("last_error".into(),serde_json::json!("Migration interrupted task execution: prior external effect is UNKNOWN; do not retry automatically."));
        }
        record
            .fields
            .insert("claimed_until".into(), serde_json::json!(0));
    } else if record.kind == "schedule_delivery_client" {
        // A stopped process does not retain delivery authority in the new route.
        record
            .fields
            .insert("expires_at".into(), serde_json::json!(0));
    }
    Ok(())
}
impl SqlDomainCodec for ScheduleCodec {
    fn domain(&self) -> &'static str {
        "schedule"
    }
    fn family(&self) -> &'static str {
        "otto.schedule"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row("SELECT COALESCE(MAX(updated_at),0) FROM items", (), |r| {
                r.get::<i64>(0)
            })
            .map(|v| v.to_string())
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        schedule::initialize_schema(connection).map_err(|e| Error::Unavailable(e.to_string()))
    }
    fn next_record(
        &self,
        connection: &Connection,
        cursor: &str,
        max_bytes: usize,
    ) -> Result<Option<(String, Record)>> {
        named::next(connection, KINDS, cursor, max_bytes, interrupted)
    }
    fn import_record(&self, connection: &Connection, record: &Record) -> Result<()> {
        named::import(connection, KINDS, record).map(|_| ())
    }
    fn validate(&self, connection: &Connection) -> Result<()> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |r| r.get(0))
            .map_err(sql_error)?;
        if version != 2 {
            return Err(Error::Invalid(
                "unsupported schedule database version".into(),
            ));
        }
        named::validate(connection, KINDS, &[])
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        TursoScheduleStore::from_connection(connection)
            .map(|_| ())
            .map_err(|e| Error::Unavailable(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Kind as ScheduleKind, NewItem, Origin, Scope, State};
    use chrono::{TimeDelta, Utc};
    #[tokio::test]
    async fn migrating_claimed_prompt_does_not_replay_an_unknown_external_effect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        let store = TursoScheduleStore::open(&path).unwrap();
        let now = Utc::now();
        let origin = Origin::terminal();
        let item = store
            .create(
                NewItem {
                    origin: origin.clone(),
                    session_id: "s".into(),
                    workspace: "/w".into(),
                    kind: ScheduleKind::Prompt,
                    body: "perform operation".into(),
                    repeat: None,
                    local_time: None,
                    due_at: now,
                },
                now,
            )
            .unwrap();
        assert_eq!(
            store
                .claim_run(&Scope::Any, Some("/w"), now)
                .unwrap()
                .unwrap()
                .id,
            item.id
        );
        drop(store);
        roundtrip_for_test(&path, &target, Arc::new(ScheduleCodec)).await;
        let source = Connection::open(&path).unwrap();
        assert!(
            source
                .query_row(
                    "SELECT claimed_until FROM items WHERE id=?1",
                    [item.id],
                    |r| r.get::<i64>(0)
                )
                .unwrap()
                > 0
        );
        let store = TursoScheduleStore::open(&target).unwrap();
        assert!(
            store
                .claim_run(&Scope::Any, Some("/w"), now + TimeDelta::hours(1))
                .unwrap()
                .is_none()
        );
        let failed = store
            .list(&origin, now)
            .unwrap()
            .into_iter()
            .find(|row| row.id == item.id)
            .unwrap();
        assert_eq!(failed.state, State::Failed);
        assert!(failed.last_error.contains("UNKNOWN"));
    }
}
