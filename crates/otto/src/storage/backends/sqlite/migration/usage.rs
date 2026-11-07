//! Usage event identity, presence and token counts. Aggregates remain queries.
use super::{
    named::{self, Kind},
    *,
};
use crate::storage::backends::sqlite::usage::{self, Store};
pub struct UsageCodec;
const KINDS: &[Kind] = &[
    Kind {
        record: &crate::usage::migration::RECORDS[0],
        table: "usage_events",
    },
    Kind {
        record: &crate::usage::migration::RECORDS[1],
        table: "provider_api_operations",
    },
];
impl SqlDomainCodec for UsageCodec {
    fn domain(&self) -> &'static str {
        "usage"
    }
    fn family(&self) -> &'static str {
        "otto.usage"
    }
    fn version(&self) -> u32 {
        crate::usage::migration::VERSION
    }
    fn supports_version(&self, version: u32) -> bool {
        version == 2 || version == self.version()
    }
    fn verification_record(&self, mut record: Record, version: u32) -> Result<Record> {
        if version == self.version() {
            return Ok(record);
        }
        if version != 2 {
            return Err(Error::Invalid("unsupported usage codec version".into()));
        }
        let mut usage_record: named::NamedRecord =
            serde_json::from_slice(&record.payload).map_err(json_error)?;
        if usage_record.kind == "usage_event" {
            for field in ["cache_write_input_tokens", "cache_write_1h_input_tokens"] {
                if usage_record.fields.remove(field) != Some(serde_json::Value::Null) {
                    return Err(Error::Invalid(
                        "legacy usage has cache-write provenance".into(),
                    ));
                }
            }
        }
        record.payload = serde_json::to_vec(&usage_record).map_err(json_error)?;
        Ok(record)
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
        let mut usage_record: named::NamedRecord =
            serde_json::from_slice(&record.payload).map_err(json_error)?;
        if usage_record.version == 1 && usage_record.kind == "usage_event" {
            for field in ["cache_write_input_tokens", "cache_write_1h_input_tokens"] {
                usage_record
                    .fields
                    .entry(field.into())
                    .or_insert(serde_json::Value::Null);
            }
        }
        let mut compatible = record.clone();
        compatible.payload = serde_json::to_vec(&usage_record).map_err(json_error)?;
        named::import(connection, KINDS, &compatible).map(|_| ())
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
    #[test]
    fn legacy_v2_usage_imports_without_invented_writes_and_verifies_original_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let filename = directory.path().join("source.db");
        let store = Store::open(&filename).unwrap();
        store
            .append(&UsageRecord {
                occurred_at: "2026-11-07T00:00:00Z".into(),
                context: Context::default(),
                kind: "provider",
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Default::default()
                },
                usage_present: true,
                cached_input_tokens_present: false,
            })
            .unwrap();
        drop(store);
        let source = Connection::open(&filename).unwrap();
        let (_, mut legacy) = UsageCodec
            .next_record(&source, "", 64 * 1024)
            .unwrap()
            .unwrap();
        let mut legacy_payload: named::NamedRecord =
            serde_json::from_slice(&legacy.payload).unwrap();
        legacy_payload.fields.remove("cache_write_input_tokens");
        legacy_payload.fields.remove("cache_write_1h_input_tokens");
        legacy.payload = serde_json::to_vec(&legacy_payload).unwrap();
        let mut destination = Connection::open_in_memory().unwrap();
        UsageCodec.initialize(&mut destination).unwrap();
        assert!(UsageCodec.supports_version(2));
        assert!(UsageCodec.supports_version(3));
        assert!(!UsageCodec.supports_version(1));
        assert!(!UsageCodec.supports_version(4));
        UsageCodec.import_record(&destination, &legacy).unwrap();
        let (_, converted) = UsageCodec
            .next_record(&destination, "", 64 * 1024)
            .unwrap()
            .unwrap();
        let reconstructed = UsageCodec
            .verification_record(converted.clone(), 2)
            .unwrap();
        assert_eq!(reconstructed, legacy);
        let mut fabricated: named::NamedRecord =
            serde_json::from_slice(&converted.payload).unwrap();
        fabricated
            .fields
            .insert("cache_write_input_tokens".into(), serde_json::json!(0));
        let mut changed = converted;
        changed.payload = serde_json::to_vec(&fabricated).unwrap();
        assert!(UsageCodec.verification_record(changed, 2).is_err());
    }

    #[tokio::test]
    async fn usage_migration_preserves_cache_writes_and_their_lifetimes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.db");
        let target = directory.path().join("target.db");
        let store = Store::open(&source).unwrap();
        store
            .append(&UsageRecord {
                occurred_at: "2026-11-07T00:00:00Z".into(),
                context: Context {
                    session_id: "cached".into(),
                    ..Default::default()
                },
                kind: "provider",
                usage_present: true,
                cached_input_tokens_present: true,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 3,
                    cached_input_tokens: 40,
                    cache_write_input_tokens: Some(20),
                    cache_write_1h_input_tokens: Some(5),
                },
            })
            .unwrap();
        let expected = store.summary(Some("cached")).unwrap();
        drop(store);
        roundtrip_for_test(&source, &target, Arc::new(UsageCodec)).await;
        let migrated = Store::open(&target).unwrap();
        assert_eq!(migrated.summary(Some("cached")).unwrap(), expected);
        assert_eq!(expected.cache_write_input_tokens, Some(20));
        assert_eq!(expected.cache_write_1h_input_tokens, Some(5));
    }

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
                    ..Default::default()
                },
            ),
            (false, Usage::default()),
            (
                true,
                Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: 0,
                    ..Default::default()
                },
            ),
        ] {
            store
                .append(&UsageRecord {
                    occurred_at: "2026-10-06T00:00:00Z".into(),
                    context: context.clone(),
                    kind: "provider",
                    cached_input_tokens_present: present,
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
