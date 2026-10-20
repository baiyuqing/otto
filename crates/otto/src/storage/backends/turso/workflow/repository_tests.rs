
use super::*;
use crate::workflow::repository::WorkflowStore;
#[test]
fn legacy_delivery_backfill_is_once_and_idle_plan_uses_pending_index() {
    let store = TursoWorkflowStore::open_in_memory();
    let connection = store.connection.lock().unwrap();
    // Simulate old schema populated before conversational delivery ledger existed.
    connection.execute("INSERT INTO workflow_runs(id,workflow,definition_json,definition_hash,workspace,profile,provider,model,input,status,created_at,updated_at) VALUES ('legacy','flow','{}','hash','/w','','','','','succeeded','now','now')",()).unwrap();
    connection.execute("INSERT INTO workflow_origins(workspace,session_id,operation_id,run_id,definition_hash,input) VALUES('/w','source','op','legacy','hash','')",()).unwrap();
    connection.execute("INSERT INTO workflow_events(seq,run_id,occurred_at,kind) VALUES(1,'legacy','now','run_succeeded')",()).unwrap();
    connection
        .execute("DELETE FROM workflow_delivery_schema", ())
        .unwrap();
    migrate_schema(&connection).unwrap();
    drop(connection);
    assert_eq!(store.pending_events("/w", "source", 64).unwrap()[0].seq, 1);
    store.acknowledge_event("/w", "source", 1).unwrap();
    let connection = store.connection.lock().unwrap();
    migrate_schema(&connection).unwrap();
    let mut query=connection.prepare("EXPLAIN QUERY PLAN SELECT e.seq FROM workflow_pending p JOIN workflow_events e ON e.seq=p.seq WHERE p.workspace='/w' AND p.session_id='source' ORDER BY p.seq LIMIT 64").unwrap();
    let plan = query
        .query_map((), |row| row.get::<String>(3))
        .unwrap()
        .collect::<crate::storage::Result<Vec<_>>>()
        .unwrap()
        .join(";");
    assert!(plan.contains("SEARCH p USING"), "{plan}");
    assert!(
        !plan.contains("SCAN e") && !plan.contains("SCAN o"),
        "idle query must not scan retained origins/events: {plan}"
    );
    drop(query);
    drop(connection);
    assert!(store.pending_events("/w", "source", 64).unwrap().is_empty());
}

#[test]
fn attempt_journal_keeps_each_transcript_path() {
    let store = TursoWorkflowStore::open_in_memory();
    let definition = Definition {
        source: String::new(),
        name: "flow".into(),
        description: String::new(),
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
        .create_run("run", &definition, "/w", "p", "provider", "m", "")
        .unwrap();
    store
        .claim_ready("run", "work", "/tmp/attempt-1.jsonl")
        .unwrap();
    store.recover("/w").unwrap();
    store.retry("run", "work").unwrap();
    store
        .claim_ready("run", "work", "/tmp/attempt-2.jsonl")
        .unwrap();
    let connection = store.connection.lock().unwrap();
    let mut statement = connection.prepare("SELECT transcript_path FROM workflow_attempts WHERE run_id = 'run' AND step_id = 'work' ORDER BY attempt").unwrap();
    let paths: Vec<String> = statement
        .query_map((), |row| row.get(0))
        .unwrap()
        .collect::<crate::storage::Result<_>>()
        .unwrap();
    assert_eq!(paths, ["/tmp/attempt-1.jsonl", "/tmp/attempt-2.jsonl"]);
}
