use super::*;
use crate::log::StopListener;
use otto_core::agent::budget::{BudgetKind, Exceeded};
use otto_core::diagnostics::TurnStop;
use std::num::NonZeroU64;

fn budget_rows(home: &Path) -> Vec<Value> {
    budget_rows_for(home, "turn_budget_exceeded")
}

fn budget_rows_for(home: &Path, event: &str) -> Vec<Value> {
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
    rows.retain(|row| row["event"] == event);
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
    scope.turn_budget_exceeded(&exceeded, "root-1");

    let rows = budget_rows(home.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["session_id"], "session-a");
    assert_eq!(rows[0]["limit"], "tokens");
    assert_eq!(rows[0]["limit_value"], 4_200_000);
    assert_eq!(rows[0]["used"], 4_200_311);
    assert_eq!(rows[0]["budget_id"], "root-1");
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
        .turn_budget_exceeded(&exceeded, "root-2");
    let rows = budget_rows(home.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["task_id"], "task-1");
    assert_eq!(rows[0]["limit"], "steps");
    assert_eq!(rows[0]["budget_id"], "root-2");
}

#[test]
fn a_skipped_wake_writes_one_record_without_notification_text_or_a_stop() {
    let home = tempfile::tempdir().unwrap();
    let log = Log::open(home.path(), "test").unwrap();
    let stops: Arc<StopListener> = Arc::default();
    let scope = log.scope("session-a", "", Redactor::new(&[]), stops.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _ = stops.set(Arc::new(move |stop| sink.lock().unwrap().push(stop)));

    scope.turn_budget_wake_skipped("root-1", 3);

    let mut rows = budget_rows_for(home.path(), "turn_budget_wake_skipped");
    assert_eq!(rows.len(), 1);
    let row = rows.remove(0);
    assert_eq!(row["session_id"], "session-a");
    assert_eq!(row["budget_id"], "root-1");
    assert_eq!(row["pending_notifications"], 3);
    for field in ["text", "notification", "task_id", "message"] {
        assert!(row.get(field).is_none(), "{field} must not be recorded");
    }
    assert!(seen.lock().unwrap().is_empty(), "no turn ran, so no stop");
}
