//! Workflow logical runs, steps, attempts, review requests and event history.
use super::{
    named::{self, Kind},
    *,
};
use crate::storage::backends::turso::workflow::{self, TursoWorkflowStore};

pub struct WorkflowCodec;
const KINDS: &[Kind] = &[
    Kind {
        record: &crate::workflow::migration::RECORDS[0],
        table: "workflow_runs",
    },
    Kind {
        record: &crate::workflow::migration::RECORDS[1],
        table: "workflow_steps",
    },
    Kind {
        record: &crate::workflow::migration::RECORDS[2],
        table: "workflow_attempts",
    },
    Kind {
        record: &crate::workflow::migration::RECORDS[3],
        table: "workflow_requests",
    },
    Kind {
        record: &crate::workflow::migration::RECORDS[4],
        table: "workflow_events",
    },
    Kind {
        record: &crate::workflow::migration::RECORDS[5],
        table: "workflow_origins",
    },
    Kind {
        record: &crate::workflow::migration::RECORDS[6],
        table: "workflow_pending",
    },
];
impl SqlDomainCodec for WorkflowCodec {
    fn domain(&self) -> &'static str {
        "workflows"
    }
    fn family(&self) -> &'static str {
        "otto.workflows"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row(
                "SELECT COALESCE(MAX(seq),0) FROM workflow_events",
                (),
                |r| r.get::<i64>(0),
            )
            .map(|v| v.to_string())
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        connection
            .execute_batch(workflow::SCHEMA)
            .map_err(sql_error)?;
        workflow::migrate_schema(connection).map_err(Error::Unavailable)
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
        named::validate(connection, KINDS, &["workflow_delivery_schema"])?;
        let broken: i64 = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM workflow_runs r LEFT JOIN workflow_runs s \
            ON s.id=r.forked_from_run_id WHERE r.forked_from_run_id IS NOT NULL AND \
            s.id IS NULL)+(SELECT COUNT(*) FROM workflow_steps s LEFT JOIN \
            workflow_steps p ON p.run_id=s.source_run_id AND p.id=s.source_step_id \
            WHERE s.source_run_id IS NOT NULL AND p.id IS NULL)",
                (),
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if broken != 0 {
            return Err(Error::Invalid(
                "workflow fork reference closure failed".into(),
            ));
        }
        let invalid_pending: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM workflow_pending p LEFT JOIN workflow_events e ON \
            e.seq=p.seq WHERE e.seq IS NULL OR NOT EXISTS(SELECT 1 FROM \
            workflow_origins o WHERE o.workspace=p.workspace AND \
            o.session_id=p.session_id AND o.run_id=e.run_id)",
                (),
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if invalid_pending != 0 {
            return Err(Error::Invalid(
                "workflow pending reference closure failed".into(),
            ));
        }
        // The ordinary repository's recovery owns Running -> Interrupted/Paused.
        // Import preserves attempts and never starts an agent or replays a tool.
        Ok(())
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        TursoWorkflowStore::from_connection(connection)
            .map(|_| ())
            .map_err(Error::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::repository::WorkflowStore;
    use crate::workflow::{Definition, RunStatus, Step, StepKind, StepStatus};
    #[tokio::test]
    async fn workflow_migration_retains_attempt_history_then_ordinary_recovery_interrupts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let target = dir.path().join("target.db");
        let store = TursoWorkflowStore::open(&path).unwrap();
        let definition = Definition {
            name: "flow".into(),
            description: String::new(),
            source: String::new(),
            steps: vec![Step {
                id: "work".into(),
                kind: StepKind::Agent,
                agent: "worker".into(),
                prompt: "work".into(),
                needs: vec![],
            }],
            agents: vec![],
            hash: "hash".into(),
        };
        store
            .create_origin_run(
                "run",
                &definition,
                "/w",
                &crate::workflow::RuntimeIdentity {
                    profile: "p".into(),
                    provider: "openai-compatible".into(),
                    model: "m".into(),
                },
                "",
                "source",
                "operation",
            )
            .unwrap();
        store
            .claim_ready("run", "work", "/tmp/attempt-1.jsonl")
            .unwrap();
        store.recover("/w").unwrap();
        store.retry("run", "work").unwrap();
        store
            .claim_ready("run", "work", "/tmp/attempt-2.jsonl")
            .unwrap();
        let expected = store.get_run("run").unwrap().unwrap();
        let events = store.events("run", 0).unwrap();
        let first = store.pending_events("/w", "source", 1).unwrap()[0].seq;
        store.acknowledge_event("/w", "source", first).unwrap();
        let pending = store.pending_events("/w", "source", 64).unwrap();
        assert!(!pending.is_empty());
        drop(store);
        roundtrip_for_test(&path, &target, Arc::new(WorkflowCodec)).await;
        let store = TursoWorkflowStore::open(&target).unwrap();
        assert_eq!(store.get_run("run").unwrap().unwrap(), expected);
        assert_eq!(store.events("run", 0).unwrap(), events);
        assert_eq!(
            store.pending_events("/w", "source", 64).unwrap(),
            pending,
            "migration preserves exact pending subset, not all retained events"
        );
        assert_eq!(store.origin_runs("/w", "source").unwrap(), ["run"]);
        assert_eq!(
            store
                .origin_run("/w", "source", "operation")
                .unwrap()
                .unwrap()
                .id,
            "run"
        );
        store.recover("/w").unwrap();
        let recovered = store.get_run("run").unwrap().unwrap();
        assert_eq!(recovered.status, RunStatus::Paused);
        assert_eq!(recovered.steps[0].status, StepStatus::Interrupted);
        assert_eq!(recovered.steps[0].attempt, 2);
        let connection = Connection::open(&target).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM workflow_attempts WHERE run_id='run'",
                    (),
                    |r| r.get::<i64>(0)
                )
                .unwrap(),
            2
        );
    }
}
