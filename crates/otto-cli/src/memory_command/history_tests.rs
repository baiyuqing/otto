use super::tests::{memory, memory_config, runtime, store_dir};
use otto::memory::{RememberRequest, Scope};

/// A workspace directory, the config file and the user scope of a fresh home.
struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    config: String,
}

fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let config = memory_config(home.path());
    Fixture {
        home,
        workspace,
        config,
    }
}

impl Fixture {
    fn cwd(&self) -> String {
        otto_platform::fsops::canonical_directory(self.workspace.path())
            .expect("canonical workspace")
            .to_string_lossy()
            .into_owned()
    }

    fn run(&self, subcommand_and_arguments: &[&str]) -> (i32, String, String) {
        let cwd = self.cwd();
        let mut arguments: Vec<&str> = subcommand_and_arguments.to_vec();
        arguments.extend(["--config", &self.config, "--cwd", &cwd]);
        memory(&arguments, self.home.path())
    }

    /// Remembers `text` under key `editor` in the workspace scope.
    fn remember(&self, text: &str) -> otto::memory::Record {
        let runtime = runtime(self.home.path());
        let (service, _, usable) =
            otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
                .expect("open memory service");
        assert!(usable, "the store must be usable");
        let scope: Scope = otto::runtime::wiring::workspace_memory_scope(&runtime, &self.cwd())
            .expect("workspace scope");
        let record = service
            .remember(&RememberRequest {
                scope,
                kind: "preference".into(),
                key: "editor".into(),
                text: text.into(),
                ..RememberRequest::default()
            })
            .expect("remember");
        service.close().expect("close");
        record
    }
}

#[test]
fn history_of_a_never_updated_record_prints_one_revision_line() {
    let fixture = fixture();
    let record = fixture.remember("vim");
    let (code, stdout, stderr) = fixture.run(&["history", &record.id]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert_eq!(stdout.lines().count(), 1, "stdout = {stdout:?}");
    assert!(
        stdout.starts_with("revision=1 ") && stdout.contains("text=vim"),
        "stdout = {stdout:?}"
    );
}

#[test]
fn history_of_an_unknown_record_fails_with_not_found() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["history", "no-such-id"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("record no-such-id not found"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn diff_of_the_current_revision_with_itself_prints_the_header() {
    let fixture = fixture();
    let record = fixture.remember("vim");
    let (code, stdout, stderr) = fixture.run(&["diff", &record.id, "1"]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.starts_with("revision 1 ("), "stdout = {stdout:?}");
    assert!(stdout.contains("-> revision 1 ("), "stdout = {stdout:?}");
}

#[test]
fn diff_names_a_revision_that_is_not_in_the_history() {
    let fixture = fixture();
    let record = fixture.remember("vim");
    let (code, _, stderr) = fixture.run(&["diff", &record.id, "1", "7"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains(&format!("record {} has no revision 7", record.id)),
        "stderr = {stderr:?}"
    );
}

#[test]
fn restore_of_the_only_revision_fails_and_names_the_revision() {
    let fixture = fixture();
    let record = fixture.remember("vim");
    let (code, stdout, stderr) = fixture.run(&["restore", &record.id, "1"]);
    assert_ne!(code, 0, "stdout = {stdout:?}");
    assert!(stderr.contains("revision 1"), "stderr = {stderr:?}");
}

#[test]
fn forget_reports_the_copies_outside_the_store() {
    let fixture = fixture();
    let record = fixture.remember("vim");
    let (code, stdout, stderr) = fixture.run(&["forget", &record.id]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(
        stdout.starts_with(&format!("forgot {} (revision 1)\n", record.id)),
        "stdout = {stdout:?}"
    );
    assert!(
        stdout.contains("  unverified: memory advice audit log"),
        "stdout = {stdout:?}"
    );
    assert!(
        stdout.contains("  present: session files"),
        "stdout = {stdout:?}"
    );
}

#[test]
fn history_diff_and_restore_with_missing_or_non_numeric_arguments_report_usage() {
    let fixture = fixture();
    let cases: [&[&str]; 8] = [
        &["history"],
        &["diff"],
        &["diff", "someid"],
        &["diff", "someid", "x"],
        &["diff", "someid", "1", "y"],
        &["restore"],
        &["restore", "someid"],
        &["restore", "someid", "latest"],
    ];
    for case in cases {
        let (code, stdout, stderr) = fixture.run(case);
        assert_ne!(code, 0, "{case:?}: stdout = {stdout:?}");
        assert!(
            stderr.contains("usage: otto memory"),
            "{case:?}: stderr = {stderr:?}"
        );
    }
    let (code, _, stderr) = fixture.run(&["restore", "someid", "0"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("usage: otto memory restore"),
        "stderr = {stderr:?}"
    );
}

/// Updates the record through the service, then reads the history through
/// the CLI. Needs the file store's history methods.
#[test]
fn history_diff_and_restore_follow_an_update() {
    let fixture = fixture();
    let first = fixture.remember("vim");
    let second = fixture.remember("neovim");
    assert_eq!(
        (second.id.as_str(), second.revision),
        (first.id.as_str(), 2)
    );

    let (code, stdout, stderr) = fixture.run(&["history", &first.id]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert_eq!(stdout.lines().count(), 2, "stdout = {stdout:?}");
    assert!(
        stdout
            .lines()
            .last()
            .is_some_and(|line| line.contains("current=yes")),
        "stdout = {stdout:?}"
    );

    let (code, stdout, stderr) = fixture.run(&["diff", &first.id, "1", "2"]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(
        stdout.contains("- vim") && stdout.contains("+ neovim"),
        "stdout = {stdout:?}"
    );

    let (code, stdout, stderr) = fixture.run(&["restore", &first.id, "1"]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert_eq!(
        stdout,
        format!("restored {} as revision 3 (from revision 1)\n", first.id)
    );

    let (code, stdout, stderr) = fixture.run(&["history", &first.id]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert_eq!(stdout.lines().count(), 3, "stdout = {stdout:?}");
    assert!(
        stdout
            .lines()
            .last()
            .is_some_and(|line| line.contains("restored_from=1")),
        "stdout = {stdout:?}"
    );
}

#[test]
fn forget_after_an_update_removes_the_history_directory() {
    let fixture = fixture();
    let first = fixture.remember("vim");
    fixture.remember("neovim");
    let history_directory = store_dir(fixture.home.path())
        .join("history")
        .join(&first.id);
    assert!(history_directory.exists(), "{history_directory:?}");
    let (code, stdout, stderr) = fixture.run(&["forget", &first.id]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("removed: "), "stdout = {stdout:?}");
    assert!(!history_directory.exists(), "{history_directory:?}");
}
