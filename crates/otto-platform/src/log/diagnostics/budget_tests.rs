use super::*;
use crate::log::StopListener;
use otto_core::agent::budget::{BudgetKind, Exceeded};
use otto_core::diagnostics::TurnStop;
use std::num::NonZeroU64;

fn budget_rows(home: &Path) -> Vec<Value> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(home.join(".otto/logs"))
        .unwrap()
        .flatten()
    {
        if entry.file_name().to_string_lossy().ends_with(".jsonl") {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            rows.extend(
                text.lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap()),
            );
        }
    }
    rows.retain(|row| row["event"] == "turn_budget_exceeded");
    rows
}

#[test]
fn a_budget_stop_writes_one_record_and_calls_the_listener_once() {
    let home = tempfile::tempdir().unwrap();
    let log = Log::open(home.path(), "test").unwrap();
    let stops: Arc<StopListener> = Arc::default();
    let scope = log.scope("session-a", "", Redactor::new(&[]), stops.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _ = stops.set(Arc::new(move |stop| sink.lock().unwrap().push(stop)));
    let exceeded = Exceeded {
        kind: BudgetKind::Tokens,
        limit: NonZeroU64::new(4_200_000).unwrap(),
        used: 4_200_311,
    };
    scope.turn_budget_exceeded(&exceeded);

    let rows = budget_rows(home.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["session_id"], "session-a");
    assert_eq!(rows[0]["limit"], "tokens");
    assert_eq!(rows[0]["limit_value"], 4_200_000);
    assert_eq!(rows[0]["used"], 4_200_311);
    assert!(rows[0].get("task_id").is_none());
    assert_eq!(
        *seen.lock().unwrap(),
        [TurnStop::Budget(BudgetKind::Tokens)]
    );
}

#[test]
fn a_child_scope_adds_the_task_id() {
    let home = tempfile::tempdir().unwrap();
    let log = Log::open(home.path(), "test").unwrap();
    let scope = log.scope("session-a", "", Redactor::new(&[]), Default::default());
    let exceeded = Exceeded {
        kind: BudgetKind::Steps,
        limit: NonZeroU64::new(54).unwrap(),
        used: 54,
    };
    scope
        .for_task("task-1")
        .unwrap()
        .turn_budget_exceeded(&exceeded);
    let rows = budget_rows(home.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["task_id"], "task-1");
    assert_eq!(rows[0]["limit"], "steps");
}
