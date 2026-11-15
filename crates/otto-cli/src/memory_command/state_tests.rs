//! Tests of `otto memory state|verify` and the lifecycle flags of `list`.
//!
//! Tests marked "needs the store" call `Service::set_state`, `Service::verify`
//! or the list filters, which the file store implements.

use super::tests::{memory, memory_config, runtime};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use otto::memory::{Record, RememberRequest};

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

    /// Remembers `text` under `key` in the workspace scope.
    fn remember(&self, key: &str, text: &str, review_at: Option<DateTime<Utc>>) -> Record {
        let runtime = runtime(self.home.path());
        let (service, _, usable) =
            otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
                .expect("open memory service");
        assert!(usable, "the store must be usable");
        let scope = otto::runtime::wiring::workspace_memory_scope(&runtime, &self.cwd())
            .expect("workspace scope");
        let record = service
            .remember(&RememberRequest {
                scope,
                kind: "note".into(),
                key: key.into(),
                text: text.into(),
                review_at,
                ..RememberRequest::default()
            })
            .expect("remember");
        service.close().expect("close");
        record
    }
}

/// The time printed after `name=` on a line, in the `Display` form of `DateTime<Utc>`.
fn printed_time(output: &str, name: &str) -> DateTime<Utc> {
    let start = output
        .find(name)
        .unwrap_or_else(|| panic!("{name} missing in {output:?}"))
        + name.len();
    let text = output[start..].get(..23).expect("time text");
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .unwrap_or_else(|error| panic!("parse {text:?}: {error}"))
        .and_utc()
}

#[test]
fn state_with_an_unknown_state_fails_with_the_accepted_names() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["state", "some-id", "bogus"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("unknown state \"bogus\""),
        "stderr = {stderr:?}"
    );
    assert!(stderr.contains("needs-review"), "stderr = {stderr:?}");
}

#[test]
fn state_superseded_without_a_replacement_fails() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["state", "some-id", "superseded"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("requires the replacement record id"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn state_other_than_superseded_rejects_a_replacement() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["state", "some-id", "retracted", "other-id"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("only state superseded takes a replacement"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn state_and_verify_without_an_id_report_usage() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["state"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("usage: otto memory state"),
        "stderr = {stderr:?}"
    );
    let (code, _, stderr) = fixture.run(&["verify"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("usage: otto memory verify"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn verify_review_in_rejects_an_absolute_time_and_a_malformed_age() {
    let fixture = fixture();
    for value in ["2030-01-01T00:00:00Z", "soon", "0d", "-3d"] {
        let (code, _, stderr) = fixture.run(&["verify", "some-id", "--review-in", value]);
        assert_eq!(code, 1, "value {value}");
        assert!(
            stderr.contains("--review-in requires an age"),
            "value {value}: stderr = {stderr:?}"
        );
    }
}

#[test]
fn list_rejects_an_unknown_state_and_an_unparsable_time() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["list", "--state", "archived"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("unknown state \"archived\""),
        "stderr = {stderr:?}"
    );
    for flag in ["--updated-after", "--verified-after"] {
        let (code, _, stderr) = fixture.run(&["list", flag, "yesterday"]);
        assert_eq!(code, 1, "{flag}");
        assert!(
            stderr.contains(&format!("{flag} requires")),
            "{flag}: stderr = {stderr:?}"
        );
    }
}

#[test]
fn list_accepts_every_lifecycle_flag_in_both_forms() {
    let fixture = fixture();
    fixture.remember("editor", "vim", None);
    let (code, _, stderr) = fixture.run(&[
        "list",
        "--state",
        "needs-review",
        "--state=retracted",
        "--kind",
        "note",
        "--label=a",
        "--updated-after",
        "30d",
        "--unverified",
        "--review-due",
    ]);
    assert_eq!(code, 0, "stderr = {stderr}");
    let (code, _, stderr) = fixture.run(&["list", "--verified-after=2020-01-01T00:00:00Z"]);
    assert_eq!(code, 0, "stderr = {stderr}");
}

#[test]
fn state_and_verify_reject_scope_all() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["state", "some-id", "retracted", "--scope", "all"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("require --scope current, user, or workspace"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn state_of_an_unknown_record_fails_with_not_found() {
    let fixture = fixture();
    let (code, _, stderr) = fixture.run(&["state", "missing", "retracted"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("record missing not found"),
        "stderr = {stderr:?}"
    );
}

// Needs the store: Service::set_state and the list state filter.
#[test]
fn needs_review_hides_a_record_from_the_default_list_and_state_all_shows_it() {
    let fixture = fixture();
    let kept = fixture.remember("kept", "stays-active", None);
    let flagged = fixture.remember("flagged", "needs-a-look", None);
    let (code, stdout, stderr) =
        fixture.run(&["state", &flagged.id, "needs-review", "--reason", "unsure"]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert_eq!(
        stdout.trim_end(),
        format!(
            "state of {} is now needs_review (revision 2) reason=unsure",
            flagged.id
        )
    );

    let (_, default_list, _) = fixture.run(&["list"]);
    assert!(
        default_list.contains(&kept.id),
        "default list = {default_list:?}"
    );
    assert!(
        !default_list.contains(&flagged.id),
        "default list = {default_list:?}"
    );

    let (code, all, stderr) = fixture.run(&["list", "--state", "all"]);
    assert_eq!(code, 0, "stderr = {stderr}");
    let line = all
        .lines()
        .find(|line| line.contains(&flagged.id))
        .expect("flagged line");
    assert!(
        line.contains(" state=needs_review ") && line.contains(" state_reason=unsure "),
        "line = {line:?}"
    );
    assert!(all.contains(&kept.id));

    let (_, only, _) = fixture.run(&["list", "--state", "needs-review"]);
    assert!(
        only.contains(&flagged.id) && !only.contains(&kept.id),
        "only = {only:?}"
    );

    let (_, shown, _) = fixture.run(&["show", &flagged.id]);
    assert!(shown.contains(" state=needs_review "), "shown = {shown:?}");
}

// Needs the store: Service::set_state superseded handling.
#[test]
fn superseded_names_its_replacement_in_the_output_and_the_list_line() {
    let fixture = fixture();
    let old = fixture.remember("old", "old-text", None);
    let new = fixture.remember("new", "new-text", None);
    let (code, stdout, stderr) = fixture.run(&["state", &old.id, "superseded", &new.id]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(
        stdout.contains("is now superseded (revision 2)")
            && stdout.contains(&format!(" superseded_by={}", new.id)),
        "stdout = {stdout:?}"
    );
    let (_, listed, _) = fixture.run(&["list", "--state", "superseded"]);
    let line = listed
        .lines()
        .find(|line| line.contains(&old.id))
        .expect("old line");
    assert!(
        line.contains(&format!(" superseded_by={} ", new.id)),
        "line = {line:?}"
    );
    let (code, _, stderr) = fixture.run(&["state", &new.id, "superseded", "missing-id"]);
    assert_eq!(code, 1);
    assert!(
        stderr.starts_with("otto: set state of "),
        "stderr = {stderr:?}"
    );
}

// Needs the store: Service::verify.
#[test]
fn verify_review_in_sets_a_future_review_deadline_and_a_current_verified_at() {
    let fixture = fixture();
    let record = fixture.remember("editor", "vim", None);
    let before = Utc::now();
    let (code, stdout, stderr) = fixture.run(&["verify", &record.id, "--review-in", "30d"]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(
        stdout.starts_with(&format!("verified {} (revision 2) verified_at=", record.id)),
        "stdout = {stdout:?}"
    );
    let verified_at = printed_time(&stdout, "verified_at=");
    let review_at = printed_time(&stdout, "review_at=");
    assert!(
        verified_at >= before - Duration::seconds(1) && verified_at <= Utc::now(),
        "stdout = {stdout:?}"
    );
    assert!(
        review_at > before + Duration::days(29),
        "review_at = {review_at}"
    );
    assert!(
        review_at < before + Duration::days(31),
        "review_at = {review_at}"
    );

    let (code, stdout, _) = fixture.run(&["verify", &record.id]);
    assert_eq!(code, 0);
    assert!(
        stdout.trim_end().ends_with("review_at=none"),
        "stdout = {stdout:?}"
    );
}

// Needs the store: the review_due, unverified and verified_after filters.
#[test]
fn list_filters_select_review_due_and_unverified_records() {
    let fixture = fixture();
    let due = fixture.remember(
        "due",
        "review-passed",
        Some(Utc::now() - Duration::hours(1)),
    );
    let later = fixture.remember(
        "later",
        "review-ahead",
        Some(Utc::now() + Duration::days(7)),
    );
    let verified = fixture.remember("verified", "was-verified", None);
    let (code, _, stderr) = fixture.run(&["verify", &verified.id]);
    assert_eq!(code, 0, "stderr = {stderr}");

    let (code, due_list, stderr) = fixture.run(&["list", "--review-due", "--state", "all"]);
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(due_list.contains(&due.id), "due list = {due_list:?}");
    assert!(
        !due_list.contains(&later.id) && !due_list.contains(&verified.id),
        "due list = {due_list:?}"
    );

    let (_, unverified, _) = fixture.run(&["list", "--unverified", "--state", "all"]);
    assert!(
        unverified.contains(&due.id) && unverified.contains(&later.id),
        "unverified = {unverified:?}"
    );
    assert!(
        !unverified.contains(&verified.id),
        "unverified = {unverified:?}"
    );

    let (_, recent, _) = fixture.run(&["list", "--verified-after", "1h", "--state", "all"]);
    assert!(
        recent.contains(&verified.id) && !recent.contains(&later.id),
        "recent = {recent:?}"
    );
}
