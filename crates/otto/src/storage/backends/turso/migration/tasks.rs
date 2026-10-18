//! Persisted task histories. Process identity is historical after cutover.
use super::{
    named::{self, Kind, NamedRecord},
    *,
};
use crate::storage::backends::turso::tasks::{self, Store};
pub struct TaskCodec;
const KINDS: &[Kind] = &[Kind {
    record: &crate::subagent::migration::RECORDS[0],
    table: "tasks",
}];
fn interrupted(record: &mut NamedRecord) -> Result<()> {
    if matches!(record.fields["status"].as_str(), Some("queued" | "running")) {
        record
            .fields
            .insert("status".into(), serde_json::json!("interrupted"));
    }
    // Keep the exact original PID/start only as history; an Interrupted record
    // never acquires live authority from a matching PID in the new deployment.
    Ok(())
}
impl SqlDomainCodec for TaskCodec {
    fn domain(&self) -> &'static str {
        "tasks"
    }
    fn family(&self) -> &'static str {
        "otto.tasks"
    }
    fn version(&self) -> u32 {
        crate::subagent::migration::VERSION
    }
    fn supports_version(&self, version: u32) -> bool {
        version == 1 || version == self.version()
    }
    fn verification_record(&self, mut record: Record, version: u32) -> Result<Record> {
        if version == self.version() {
            return Ok(record);
        }
        if version != 1 {
            return Err(Error::Invalid("unsupported task codec version".into()));
        }
        let mut data: NamedRecord = serde_json::from_slice(&record.payload).map_err(json_error)?;
        // A legacy source has no routing provenance. Do not hide a changed value
        // from digest verification merely because its descriptor predates it.
        if data.fields.remove("model_source") != Some(serde_json::json!("unknown")) {
            return Err(Error::Invalid(
                "legacy task has model source provenance".into(),
            ));
        }
        record.payload = serde_json::to_vec(&data).map_err(json_error)?;
        Ok(record)
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row("SELECT COALESCE(MAX(created_at),'') FROM tasks", (), |r| {
                r.get(0)
            })
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        named::initialize_schema(connection, tasks::SCHEMA_VERSION, tasks::SCHEMA)
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
        let mut data: NamedRecord = serde_json::from_slice(&record.payload).map_err(json_error)?;
        if data.version == 1 && data.kind == "subagent_task" {
            data.fields
                .entry("model_source".into())
                .or_insert_with(|| serde_json::json!("unknown"));
        }
        let mut compatible = record.clone();
        compatible.payload = serde_json::to_vec(&data).map_err(json_error)?;
        named::import(connection, KINDS, &compatible).map(|_| ())
    }
    fn validate(&self, connection: &Connection) -> Result<()> {
        named::version(connection, tasks::SCHEMA_VERSION)?;
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
    use crate::subagent::{
        record::{Recorder, TaskContext, current_process},
        tasks::{Task, TaskStatus},
    };
    #[tokio::test]
    async fn legacy_portable_task_import_defaults_model_source_to_unknown() {
        // Construct the v1 export shape from the real logical exporter, with a
        // descriptor and checksums over the original bytes (not normalized v2).
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let source_store = Store::open(&source).unwrap();
        source_store.upsert(
            &TaskContext {
                parent_session: "parent".into(),
                ..Default::default()
            },
            &Task {
                id: "t1".into(),
                ..Default::default()
            },
        );
        drop(source_store);
        let connection = Connection::open(&source).unwrap();
        let (_, mut record) = TaskCodec
            .next_record(&connection, "", 1024 * 1024)
            .unwrap()
            .unwrap();
        let mut data: NamedRecord = serde_json::from_slice(&record.payload).unwrap();
        data.fields.remove("model_source");
        record.payload = serde_json::to_vec(&data).unwrap();
        assert_eq!(data.version, 1); // Logical record envelopes remain v1.
        let bytes = serde_json::to_vec(&record).unwrap();
        let mut hash = Sha256::new();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
        let digest = DomainDigest {
            records: 1,
            blob_bytes: 0,
            sha256: format!("{:x}", hash.finalize()),
        };
        let chunk = Chunk {
            cursor: String::new(),
            next_cursor: None,
            records: vec![record],
            blobs: vec![],
        };
        let mut manifest = Manifest {
            format_version: FORMAT_VERSION,
            max_chunk_bytes: 1024 * 1024,
            migration_id: "legacy-task".into(),
            destination_identity: "target".into(),
            expected_route_generation: "route-1".into(),
            source: FrozenSnapshot {
                source_identity: "source".into(),
                generation: "frozen".into(),
                domains: BTreeMap::from([(
                    "tasks".into(),
                    DomainSnapshot {
                        schema_family: "otto.tasks".into(),
                        schema_version: 1,
                        committed_cursor: "legacy".into(),
                        digest: digest.clone(),
                    },
                )]),
            },
            progress: BTreeMap::from([(
                "tasks".into(),
                Progress {
                    cursor: String::new(),
                    complete: false,
                    chunks: vec![],
                },
            )]),
            destination_write_started: true,
            sealed: false,
            published_generation: None,
            rolled_back: false,
        };
        // Exercise serialized portable metadata and payload, not import_record.
        manifest = serde_json::from_slice(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        let chunk: Chunk = serde_json::from_slice(&serde_json::to_vec(&chunk).unwrap()).unwrap();
        let target = dir.path().join("target.db");
        let stage = Stage::new(target.clone(), "target".into(), Arc::new(TaskCodec));
        for (family, version) in [("otto.tasks", 0), ("otto.tasks", 3), ("other", 1)] {
            let mut incompatible = manifest.clone();
            let domain = incompatible.source.domains.get_mut("tasks").unwrap();
            domain.schema_family = family.into();
            domain.schema_version = version;
            assert!(stage.validate_plan(&incompatible).await.is_err());
        }
        // The default policy for unrelated codecs still requires exact equality.
        assert!(ApprovalCodec.supports_version(ApprovalCodec.version()));
        assert!(!ApprovalCodec.supports_version(ApprovalCodec.version() + 1));
        assert!(!ApprovalCodec.supports_version(0));
        stage.validate_plan(&manifest).await.unwrap();
        stage.begin(&manifest).await.unwrap();
        assert!(
            stage
                .apply_chunk("legacy-task", "tasks", &chunk, "wrong")
                .await
                .is_err()
        );
        let mut malformed = chunk.clone();
        let mut data: NamedRecord = serde_json::from_slice(&malformed.records[0].payload).unwrap();
        data.fields
            .insert("untrusted_extra".into(), serde_json::json!("value"));
        malformed.records[0].payload = serde_json::to_vec(&data).unwrap();
        let hash = chunk_hash(&malformed, manifest.max_chunk_bytes).unwrap();
        assert!(
            stage
                .apply_chunk("legacy-task", "tasks", &malformed, &hash)
                .await
                .is_err()
        );
        let hash = chunk_hash(&chunk, manifest.max_chunk_bytes).unwrap();
        let receipt = stage
            .apply_chunk("legacy-task", "tasks", &chunk, &hash)
            .await
            .unwrap();
        let progress = manifest.progress.get_mut("tasks").unwrap();
        progress.complete = true;
        progress.chunks.push(receipt);
        assert_eq!(stage.verify(&manifest).await.unwrap()["tasks"], digest);
        let mut corrupted = manifest.clone();
        corrupted
            .source
            .domains
            .get_mut("tasks")
            .unwrap()
            .digest
            .sha256 = "wrong".into();
        assert!(stage.seal(&corrupted).await.is_err());
        stage.seal(&manifest).await.unwrap();
        stage.close().await.unwrap();
        // A restart must verify using the original descriptor and persisted receipts.
        let stage = Stage::new(target.clone(), "target".into(), Arc::new(TaskCodec));
        stage.validate_plan(&manifest).await.unwrap();
        assert_eq!(stage.verify(&manifest).await.unwrap()["tasks"], digest);
        stage.close().await.unwrap();
        let imported = Store::open(&target)
            .unwrap()
            .get("parent", "t1")
            .unwrap()
            .unwrap();
        assert_eq!(
            imported.model_source,
            crate::subagent::tasks::ModelSource::Unknown
        );
        assert_eq!(TaskCodec.version(), 2);
    }

    #[tokio::test]
    async fn migrating_a_task_never_reuses_its_process_identity_as_live_authority() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        let store = Store::open(&source).unwrap();
        let (pid, process_started_at) = current_process();
        let context = TaskContext {
            parent_session: "parent".into(),
            workspace: "/w".into(),
            pid,
            process_started_at: process_started_at.clone(),
            ..Default::default()
        };
        store.upsert(
            &context,
            &Task {
                id: "task-1".into(),
                name: "work".into(),
                status: TaskStatus::Running,
                model_source: crate::subagent::tasks::ModelSource::ConfiguredFirst,
                ..Default::default()
            },
        );
        drop(store);
        roundtrip_for_test(&source, &target, Arc::new(TaskCodec)).await;
        let migrated = Store::open(&target).unwrap();
        assert_eq!(
            migrated.get("parent", "task-1").unwrap().unwrap().status,
            "interrupted"
        );
        assert_eq!(
            migrated
                .get("parent", "task-1")
                .unwrap()
                .unwrap()
                .model_source,
            crate::subagent::tasks::ModelSource::ConfiguredFirst
        );
        let connection = Connection::open(&target).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT pid,process_started_at FROM tasks", (), |r| Ok((
                    r.get::<i64>(0)?,
                    r.get::<String>(1)?
                )))
                .unwrap(),
            (pid, process_started_at)
        );
        let source = Connection::open(&source).unwrap();
        assert_eq!(
            source
                .query_row("SELECT status FROM tasks", (), |r| r.get::<String>(0))
                .unwrap(),
            "running"
        );
    }
}
