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
        1
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
        named::import(connection, KINDS, record).map(|_| ())
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
