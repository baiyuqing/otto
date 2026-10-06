//! Usage event identity, presence and token counts. Aggregates remain queries.
use super::{
    named::{self, Kind},
    *,
};
use crate::storage::backends::turso::usage::{self, Store};
pub struct UsageCodec;
const KINDS: &[Kind] = &[Kind {
    name: "usage_event",
    table: "usage_events",
    fields: &[
        "id",
        "occurred_at",
        "workspace",
        "session_id",
        "task_id",
        "provider",
        "profile",
        "model",
        "kind",
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "usage_present",
    ],
    keys: &["id"],
    revision: None,
}];
impl SqlDomainCodec for UsageCodec {
    fn domain(&self) -> &'static str {
        "usage"
    }
    fn family(&self) -> &'static str {
        "otto.usage"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row("SELECT COALESCE(MAX(id),0) FROM usage_events", (), |r| {
                r.get::<i64>(0)
            })
            .map(|v| v.to_string())
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        named::initialize_schema(connection, 0, usage::SCHEMA)
    }
    fn next_record(
        &self,
        connection: &Connection,
        cursor: &str,
        max_bytes: usize,
    ) -> Result<Option<(String, Record)>> {
        named::next(connection, KINDS, cursor, max_bytes, named::unchanged)
    }
    fn import_record(&self, connection: &Connection, record: &Record) -> Result<()> {
        named::import(connection, KINDS, record).map(|_| ())
    }
    fn validate(&self, connection: &Connection) -> Result<()> {
        named::version(connection, 0)?;
        named::validate(connection, KINDS, &[])
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        Store::from_connection(connection)
            .map(|_| ())
            .map_err(|e| Error::Unavailable(e.to_string()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Context, Sink, UsageRecord};
    use otto_core::model::Usage;
    #[tokio::test]
    async fn usage_migration_preserves_presence_ids_and_weighted_aggregates() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        let store = Store::open(&source).unwrap();
        let context = Context {
            workspace: "/w".into(),
            session_id: "s".into(),
            task_id: String::new(),
            provider: "openai-compatible".into(),
            profile: "default".into(),
            model: "m".into(),
        };
        for (present, usage) in [
            (
                true,
                Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 40,
                },
            ),
            (false, Usage::default()),
            (
                true,
                Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: 0,
                },
            ),
        ] {
            store
                .append(&UsageRecord {
                    occurred_at: "2026-10-06T00:00:00Z".into(),
                    context: context.clone(),
                    kind: "provider",
                    usage,
                    usage_present: present,
                })
                .unwrap();
        }
        let expected = store.summary(Some("s")).unwrap();
        drop(store);
        roundtrip_for_test(&source, &target, Arc::new(UsageCodec)).await;
        let migrated = Store::open(&target).unwrap();
        assert_eq!(migrated.summary(Some("s")).unwrap(), expected);
        let connection = Connection::open(&target).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT group_concat(id,',') FROM usage_events", (), |r| {
                    r.get::<String>(0)
                })
                .unwrap(),
            "1,2,3"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM usage_events WHERE usage_present=0",
                    (),
                    |r| r.get::<i64>(0)
                )
                .unwrap(),
            1
        );
    }
}
