use super::*;

#[test]
fn followups_are_bounded_fifo_and_scoped_to_the_logical_run() {
    let control = Arc::new(TaskControl::default());
    let run = control.begin("session-1", "original task").unwrap();
    assert!(control.begin("session-1", "second task").is_err());
    control
        .queue_followup("survives refused second prompt")
        .unwrap();
    for i in 0..MAX_FOLLOWUPS - 1 {
        control.queue_followup(&format!("task {i}")).unwrap();
    }
    assert!(control.queue_followup("overflow").is_err());
    assert_eq!(
        run.next_followup().map(|followup| followup.text).as_deref(),
        Some("survives refused second prompt")
    );
    drop(run);
    assert!(control.queue_followup("stale").is_err());
}

#[test]
fn followup_input_is_validated_after_trimming() {
    assert_eq!(checked_text("  task  ").unwrap(), "task");
    assert!(checked_text(" \n").is_err());
    assert!(checked_text(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
}

#[test]
fn current_phase_preserves_the_original_objective_and_session_boundary() {
    let control = Arc::new(TaskControl::default());
    let run = control.begin("session-1", "research the topic").unwrap();
    run.set_current_task("write the final report");
    assert_eq!(
        control.original_task("session-1").as_deref(),
        Some("research the topic")
    );
    assert_eq!(
        control.current_task("session-1").as_deref(),
        Some("write the final report")
    );
    assert!(control.current_task("another-session").is_none());
    drop(run);
    let _next = control.begin("session-1", "a new task").unwrap();
    assert_eq!(
        control.current_task("session-1").as_deref(),
        Some("a new task")
    );
}

fn journaled() -> (Arc<TaskControl>, Arc<otto_core::mailbox::InMemoryMailbox>) {
    let journal = Arc::new(otto_core::mailbox::InMemoryMailbox::default());
    let control = Arc::new(TaskControl::default());
    control.set_followup_journal(journal.clone());
    (control, journal)
}

#[test]
fn followups_are_journaled_until_their_step_runs_or_the_task_ends() {
    let (control, journal) = journaled();
    let run = control.begin("s1", "task").unwrap();
    control.queue_followup("first").unwrap();
    control.queue_followup("second").unwrap();
    assert_eq!(journal.len(), 2);
    let first = run.next_followup().unwrap();
    assert!(first.input_id.starts_with("followup:") && first.input_id.ends_with(":1"));
    run.followup_done(&first);
    assert_eq!(journal.len(), 1);
    drop(run);
    assert!(journal.is_empty(), "the task's end discards the rest");
}

#[test]
fn a_resuming_run_adopts_the_interrupted_tasks_followups_in_order() {
    let (control, journal) = journaled();
    let run = control.begin("s1", "task").unwrap();
    control.queue_followup("first").unwrap();
    control.queue_followup("second").unwrap();
    // Process loss: the run never ends.
    std::mem::forget(run);

    let restarted = Arc::new(TaskControl::default());
    restarted.set_followup_journal(journal.clone());
    let run = restarted.begin_resuming("s1", "", true).unwrap();
    assert_eq!(run.discarded(), 0);
    let texts: Vec<String> = std::iter::from_fn(|| run.next_followup())
        .map(|followup| followup.text)
        .collect();
    assert_eq!(texts, ["first", "second"]);
    assert_eq!(journal.len(), 2, "entries stay until their steps run");
}

#[test]
fn a_new_task_discards_and_counts_an_unresumed_tasks_followups() {
    let (control, journal) = journaled();
    let run = control.begin("s1", "task").unwrap();
    control.queue_followup("first").unwrap();
    std::mem::forget(run);

    let restarted = Arc::new(TaskControl::default());
    restarted.set_followup_journal(journal.clone());
    let run = restarted.begin("s1", "new task").unwrap();
    assert_eq!(run.discarded(), 1);
    assert!(run.next_followup().is_none());
    assert!(journal.is_empty());
}

#[test]
fn a_followup_that_cannot_be_saved_is_not_queued() {
    let journal = Arc::new(otto_core::mailbox::InMemoryMailbox::default());
    journal.set_persist(Box::new(|_| Err("disk full".into())));
    let control = Arc::new(TaskControl::default());
    control.set_followup_journal(journal.clone());
    let run = control.begin("s1", "task").unwrap();
    assert!(control.queue_followup("first").is_err());
    assert!(run.next_followup().is_none());
    assert!(journal.is_empty());
}
