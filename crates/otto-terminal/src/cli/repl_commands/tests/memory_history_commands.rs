//! The `/memory` history, diff and restore commands in the REPL.

use super::*;

#[test]
fn the_memory_usage_names_history_diff_and_restore() {
    let usage = otto::app::management::Request::parse("memory", "")
        .expect_err("bare /memory must return usage");
    for form in [
        "/memory history <id>",
        "/memory diff <id> <from> [<to>]",
        "/memory restore <id> <revision>",
    ] {
        assert!(usage.contains(form), "usage lacks {form}: {usage}");
    }
}

#[tokio::test]
async fn the_repl_runs_history_diff_and_restore_against_the_store() {
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
        "/remember --scope user --kind note --key editor vim\n\
         /remember --scope user --kind note --key editor emacs\n/exit\n",
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
        &format!("/memory history {id}\n/memory diff {id} 1 2\n/memory restore {id} 1\n/exit\n"),
        &controller,
    )
    .await;

    assert!(stderr.is_empty(), "{stderr}");
    assert!(stdout.contains("revision=1 "), "{stdout}");
    assert!(stdout.contains("revision=2 "), "{stdout}");
    assert!(stdout.contains("current=yes"), "{stdout}");
    assert!(
        stdout.contains("- vim") && stdout.contains("+ emacs"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("restored {id} as revision 3 (from revision 1)")),
        "{stdout}"
    );
}
