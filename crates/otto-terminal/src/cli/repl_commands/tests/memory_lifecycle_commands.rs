//! The `/memory state` and `/memory verify` commands in the REPL.

use super::*;

#[test]
fn the_memory_usage_names_state_verify_and_the_list_filters() {
    let usage = otto::app::management::Request::parse("memory", "")
        .expect_err("bare /memory must return usage");
    for form in [
        "/memory state <id> active|needs-review|retracted [--reason TEXT]",
        "/memory state <id> superseded <replacement-id> [--reason TEXT]",
        "/memory verify <id> [--review-in AGE]",
        "[--state active|needs-review|superseded|retracted|all]",
        "[--unverified] [--review-due]",
    ] {
        assert!(usage.contains(form), "usage lacks {form}: {usage}");
    }
    let remember_usage = otto::app::management::Request::parse("remember", "")
        .expect_err("bare /remember must return usage");
    assert!(
        remember_usage.contains("[--review-in AGE]"),
        "{remember_usage}"
    );
}

// Needs the store implementation of `set_state` and `verify`.
#[tokio::test]
async fn the_repl_runs_state_and_verify_against_the_store() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    let (stdout, stderr) = session(
        "/remember --scope user --kind note --key editor vim\n/exit\n",
        &controller,
    )
    .await;
    assert!(stderr.is_empty(), "{stderr}");
    let id = stdout
        .split_whitespace()
        .skip_while(|word| *word != "remembered")
        .nth(1)
        .expect("record id")
        .to_owned();
    let (stdout, stderr) = session(
        &format!("/memory state {id} needs-review\n/memory list --scope user\n/memory list --scope user --state needs-review\n/memory verify {id}\n/exit\n"),
        &controller,
    )
    .await;
    assert!(stderr.is_empty(), "{stderr}");
    assert!(
        stdout.contains(&format!("state of {id} is now needs_review (revision 2)")),
        "{stdout}"
    );
    assert!(stdout.contains("no records"), "{stdout}");
    assert!(stdout.contains("state=needs_review"), "{stdout}");
    assert!(
        stdout.contains(&format!("verified {id} (revision 3)")),
        "{stdout}"
    );
}
