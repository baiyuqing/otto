use super::*;
use crate::log::StopListener;
use otto_core::agent::progress::{Stall, StallKind};
use otto_core::diagnostics::TurnStop;

fn stalls(home: &Path) -> Vec<Value> {
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
    rows.retain(|row| row["event"] == "turn_stalled");
    rows
}

#[test]
fn a_stalled_turn_writes_one_record_with_ids_kind_tool_and_count_only() {
    let home = tempfile::tempdir().unwrap();
    let log = Log::open(home.path(), "test").unwrap();
    let scope = log.scope("session-a", "", Redactor::new(&[]), Default::default());
    let stall = Stall {
        kind: StallKind::RepeatedCycle,
        tool: "read".into(),
        count: 8,
    };
    scope.turn_stalled(&stall);
    scope.for_task("task-1").unwrap().turn_stalled(&stall);

    let rows = stalls(home.path());
    assert_eq!(rows.len(), 2);
    let main = rows
        .iter()
        .find(|row| row.get("task_id").is_none())
        .unwrap();
    assert_eq!(main["session_id"], "session-a");
    assert_eq!(main["kind"], "repeated_cycle");
    assert_eq!(main["tool"], "read");
    assert_eq!(main["count"], 8);
    let child = rows
        .iter()
        .find(|row| row.get("task_id").is_some())
        .unwrap();
    assert_eq!(child["task_id"], "task-1");
}

#[test]
fn the_registered_listener_is_called_once_per_stall_including_child_scopes() {
    let home = tempfile::tempdir().unwrap();
    let log = Log::open(home.path(), "test").unwrap();
    let stops: Arc<StopListener> = Arc::default();
    let scope = log.scope("session-a", "", Redactor::new(&[]), stops.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    // Registered after the scope exists.
    let _ = stops.set(Arc::new(move |stop| sink.lock().unwrap().push(stop)));
    let stall = Stall {
        kind: StallKind::RepeatedFailure,
        tool: "bash".into(),
        count: 12,
    };
    scope.turn_stalled(&stall);
    scope.for_task("task-1").unwrap().turn_stalled(&stall);
    let expected = TurnStop::NoProgress(StallKind::RepeatedFailure);
    assert_eq!(*seen.lock().unwrap(), [expected, expected]);
}
