//! Memory identities, records/tombstones, observations and reviewed receipts.
//! Search copies are rebuilt from the validated records through the normal codec.
use super::{
    named::{self, Kind},
    *,
};
use crate::storage::backends::turso::memory::{self, candidates, records, schema};

pub struct MemoryCodec;
const KINDS: &[Kind] = &[
    Kind {
        record: &crate::memory::migration::RECORDS[0],
        table: "memory_meta",
    },
    Kind {
        record: &crate::memory::migration::RECORDS[1],
        table: "memory_records",
    },
    Kind {
        record: &crate::memory::migration::RECORDS[2],
        table: "memory_observations",
    },
    Kind {
        record: &crate::memory::migration::RECORDS[3],
        table: "memory_candidates",
    },
];
fn memory_error(error: crate::memory::Error) -> Error {
    Error::Invalid(error.to_string())
}
impl SqlDomainCodec for MemoryCodec {
    fn domain(&self) -> &'static str {
        "memory"
    }
    fn family(&self) -> &'static str {
        "otto.memory"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row(
                "SELECT value FROM memory_meta WHERE key='generation'",
                (),
                |r| r.get(0),
            )
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        crate::storage::sql::require(
            connection,
            crate::storage::SqlRequirements {
                fts: true,
                ..crate::storage::SqlRequirements::EPHEMERAL
            },
        )
        .map_err(sql_error)?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |r| r.get(0))
            .map_err(sql_error)?;
        if version == 2 {
            return Ok(());
        }
        if version != 0 {
            return Err(Error::Invalid("unsupported memory staging schema".into()));
        }
        let tx = connection.transaction().map_err(sql_error)?;
        for statement in schema::SCHEMA_STATEMENTS {
            tx.execute_batch(statement).map_err(sql_error)?;
        }
        tx.execute_batch("PRAGMA user_version=2")
            .map_err(sql_error)?;
        tx.commit().map_err(sql_error)
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
        let data = named::import(connection, KINDS, record)?;
        if data.kind == "memory_record" && data.fields["state"] == "active" {
            let id = data.fields["id"]
                .as_str()
                .ok_or_else(|| Error::Invalid("invalid memory identity".into()))?;
            let record = connection
                .query_row(
                    &format!(
                        "SELECT {} FROM memory_records WHERE id=?1",
                        memory::query::record_projection("")
                    ),
                    [id],
                    |r| Ok(records::decode_record_row(r)),
                )
                .map_err(sql_error)?
                .map_err(memory_error)?;
            records::insert_fts(connection, &record).map_err(memory_error)?;
        }
        Ok(())
    }
    fn validate(&self, connection: &Connection) -> Result<()> {
        named::validate(
            connection,
            KINDS,
            &[
                "memory_records_fts",
                "__turso_internal_fts_dir_memory_records_search",
            ],
        )?;
        schema::verify_schema(connection).map_err(memory_error)?;
        memory::verify_fts_integrity(connection).map_err(memory_error)?;
        for (sql, tombstone) in [
            (
                format!(
                    "SELECT {} FROM memory_records WHERE state='active'",
                    memory::query::record_projection("")
                ),
                false,
            ),
            (
                format!(
                    "SELECT {} FROM memory_records WHERE state='tombstone'",
                    memory::query::tombstone_projection()
                ),
                true,
            ),
        ] {
            let mut statement = connection.prepare(&sql).map_err(sql_error)?;
            let mut rows = statement.query(()).map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                if tombstone {
                    records::decode_tombstone_row(row).map_err(memory_error)?;
                } else {
                    records::decode_record_row(row).map_err(memory_error)?;
                }
            }
        }
        let mut statement = connection
            .prepare(&format!(
                "SELECT {} FROM memory_candidates",
                candidates::candidate_projection()
            ))
            .map_err(sql_error)?;
        let mut rows = statement.query(()).map_err(sql_error)?;
        while let Some(row) = rows.next().map_err(sql_error)? {
            candidates::decode_candidate_row(row).map_err(memory_error)?;
        }
        let broken:i64=connection.query_row("SELECT COUNT(*) FROM memory_candidates c LEFT JOIN memory_records r ON r.id=c.result_record_id WHERE c.state='accepted' AND (r.id IS NULL OR r.revision<c.result_revision OR r.scope_namespace<>c.scope_namespace OR r.scope_id<>c.scope_id)",(),|r|r.get(0)).map_err(sql_error)?;
        if broken != 0 {
            return Err(Error::Invalid(
                "memory review receipt reference closure failed".into(),
            ));
        }
        Ok(())
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        memory::Store::from_connection(connection, memory::Options::default())
            .map(|_| ())
            .map_err(memory_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{RecordRef, StoreForgetRequest, UpsertRequest};
    use crate::storage::backends::turso::memory::records::testsupport::{at, sample_record};
    #[test]
    fn memory_schema_accepts_only_exact_migration_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        memory::Store::open(&path, memory::Options::default())
            .unwrap()
            .close()
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(STAGE_SCHEMA).unwrap();
        assert!(schema::verify_schema(&connection).is_ok());
        connection
            .execute_batch("CREATE TABLE unexpected_state (id TEXT)")
            .unwrap();
        assert!(schema::verify_schema(&connection).is_err());
        connection.execute_batch("DROP TABLE unexpected_state; DROP TABLE otto_migration_chunks; CREATE TABLE otto_migration_chunks(migration_id TEXT,domain TEXT,cursor TEXT,body TEXT,PRIMARY KEY(migration_id,domain,cursor))").unwrap();
        assert!(schema::verify_schema(&connection).is_err());
    }
    #[tokio::test]
    async fn memory_migration_preserves_identity_revision_tombstone_and_rebuilds_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        let store = memory::Store::open(&path, memory::Options::default()).unwrap();
        let scope = store.identity().unwrap().user_scope;
        let active = store
            .upsert(&UpsertRequest {
                record: sample_record("active", &scope, "editor", "uses neovim"),
                expected_revision: None,
            })
            .unwrap();
        let forgotten = store
            .upsert(&UpsertRequest {
                record: sample_record("forgotten", &scope, "old", "old preference"),
                expected_revision: None,
            })
            .unwrap();
        let tombstone = store
            .forget(&StoreForgetRequest {
                reference: RecordRef {
                    scope: scope.clone(),
                    id: forgotten.id.clone(),
                },
                expected_revision: forgotten.revision,
                forgotten_at: at(1),
            })
            .unwrap();
        let identity = store.identity().unwrap();
        store.close().unwrap();
        roundtrip_for_test(&path, &target, Arc::new(MemoryCodec)).await;
        let migrated = memory::Store::open(&target, memory::Options::default()).unwrap();
        assert_eq!(migrated.identity().unwrap(), identity);
        assert_eq!(
            migrated
                .get(&RecordRef {
                    scope: scope.clone(),
                    id: active.id.clone()
                })
                .unwrap(),
            active
        );
        assert_eq!(
            migrated
                .get_tombstone(&RecordRef {
                    scope,
                    id: forgotten.id
                })
                .unwrap(),
            tombstone
        );
        migrated.close().unwrap();
        let connection = Connection::open(&target).unwrap();
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM memory_records_fts WHERE record_id='active' AND text_value='uses neovim'",(),|r|r.get::<i64>(0)).unwrap(),1);
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM memory_records_fts WHERE record_id='forgotten'",
                    (),
                    |r| r.get::<i64>(0)
                )
                .unwrap(),
            0
        );
    }
}
