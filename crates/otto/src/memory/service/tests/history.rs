//! Tests for record history, restore and the forget cleanup report.

use super::*;
use crate::memory::{CopyStatus, RESTORED_FROM_REVISION_METADATA, RestoreRequest};

fn reference_of(scope: &Scope, record: &Record) -> RecordRef {
    RecordRef {
        scope: scope.clone(),
        id: record.id.clone(),
    }
}

fn forget_request(reference: &RecordRef, expected_revision: u64) -> ForgetRequest {
    ForgetRequest {
        reference: reference.clone(),
        expected_revision,
        purge_backups: false,
        confirm_purge: false,
    }
}

#[test]
fn history_of_an_unknown_record_is_not_found() {
    let (_directory, service) = service();
    let error = service
        .history(&RecordRef {
            scope: user_scope(),
            id: "missing".into(),
        })
        .expect_err("unknown record");
    assert!(error.is(ErrorKind::NotFound), "error = {error}");
}

#[test]
fn restore_rejects_a_revision_that_does_not_precede_the_current_one() {
    let (_directory, service) = service();
    let scope = user_scope();
    let created = service
        .remember(&remember(&scope, "editor", "vim"))
        .expect("create");
    let reference = reference_of(&scope, &created);
    for (revision, expected_revision) in [(0, 2), (1, 1), (3, 2), (1, 0)] {
        let error = service
            .restore(&RestoreRequest {
                reference: reference.clone(),
                revision,
                expected_revision,
            })
            .expect_err("invalid restore");
        assert!(
            error.is(ErrorKind::InvalidRequest),
            "revision {revision} expected {expected_revision}: {error}"
        );
    }
}

#[test]
fn restore_validation_runs_before_the_store_is_reached() {
    // The alternate store answers every restore with Unsupported, so an
    // InvalidRequest proves the service rejected the request itself.
    let (_directory, store) = open_temp();
    let service = Service::from_store(Arc::new(AlternateStore(store)), decide_default_policy);
    let error = service
        .restore(&RestoreRequest {
            reference: RecordRef {
                scope: user_scope(),
                id: "r1".into(),
            },
            revision: 2,
            expected_revision: 2,
        })
        .expect_err("revision equals the current revision");
    assert!(error.is(ErrorKind::InvalidRequest), "error = {error}");
    let valid = service
        .restore(&RestoreRequest {
            reference: RecordRef {
                scope: user_scope(),
                id: "r1".into(),
            },
            revision: 1,
            expected_revision: 2,
        })
        .expect_err("the alternate store does not restore");
    assert!(valid.is(ErrorKind::Unsupported), "error = {valid}");
}

#[test]
fn forget_ends_its_cleanup_with_the_entries_for_copies_outside_the_store() {
    let (_directory, service) = service();
    let scope = user_scope();
    let created = service
        .remember(&remember(&scope, "editor", "vim"))
        .expect("create");

    let result = service
        .forget(&forget_request(
            &reference_of(&scope, &created),
            created.revision,
        ))
        .expect("forget");

    let entries = &result.cleanup;
    assert!(entries.len() >= 2, "{entries:?}");
    let advice_log = &entries[entries.len() - 2];
    let session_files = &entries[entries.len() - 1];
    assert!(
        advice_log.copy.contains("advice audit log"),
        "{advice_log:?}"
    );
    assert_eq!(advice_log.status, CopyStatus::Unverified);
    assert_eq!(session_files.copy, "session files");
    assert_eq!(session_files.status, CopyStatus::Present);
    for entry in entries {
        assert!(
            !entry.detail.contains("vim"),
            "detail holds record text: {entry:?}"
        );
    }
}

#[test]
fn forget_reports_each_store_copy_before_the_service_entries() {
    let (_directory, service) = service();
    let scope = user_scope();
    let created = service
        .remember(&remember(&scope, "editor", "vim"))
        .expect("create");
    let updated = service
        .remember(&remember(&scope, "editor", "emacs"))
        .expect("update");
    assert_eq!(updated.revision, 2);

    let result = service
        .forget(&forget_request(
            &reference_of(&scope, &created),
            updated.revision,
        ))
        .expect("forget");

    let store_entries = &result.cleanup[..result.cleanup.len() - 2];
    assert!(
        store_entries
            .iter()
            .any(|entry| entry.copy.contains("history")),
        "the store reports its history copies: {:?}",
        result.cleanup
    );
    assert!(
        store_entries
            .iter()
            .all(|entry| matches!(entry.status, CopyStatus::Removed | CopyStatus::Suppressed)),
        "every store copy of a forgotten record is removed or suppressed: {store_entries:?}"
    );
}

#[test]
fn history_lists_revisions_ascending_and_restore_adds_a_human_revision() {
    let (_directory, service) = service();
    let scope = user_scope();
    let created = service
        .remember(&remember(&scope, "editor", "vim"))
        .expect("create");
    service
        .remember(&remember(&scope, "editor", "emacs"))
        .expect("second revision");
    let current = service
        .remember(&remember(&scope, "editor", "helix"))
        .expect("third revision");
    assert_eq!(current.revision, 3);
    let reference = reference_of(&scope, &created);

    let history = service.history(&reference).expect("history");
    let listed: Vec<(u64, &str)> = history
        .revisions
        .iter()
        .map(|record| (record.revision, record.text.as_str()))
        .collect();
    assert_eq!(listed, [(1, "vim"), (2, "emacs"), (3, "helix")]);
    assert!(history.skipped.is_empty(), "{:?}", history.skipped);

    let restored = service
        .restore(&RestoreRequest {
            reference: reference.clone(),
            revision: 1,
            expected_revision: 3,
        })
        .expect("restore");
    assert_eq!(restored.revision, 4);
    assert_eq!(restored.text, "vim");
    assert_eq!(restored.source.origin, Some(Origin::Human));
    assert_eq!(
        restored
            .metadata
            .get(RESTORED_FROM_REVISION_METADATA)
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(service.get(&reference).expect("current").revision, 4);
    let after = service.history(&reference).expect("history after restore");
    assert_eq!(after.revisions.len(), 4);
    assert_eq!(after.revisions.last().expect("last").revision, 4);
}

#[test]
fn restore_with_a_stale_expected_revision_is_a_conflict() {
    let (_directory, service) = service();
    let scope = user_scope();
    let created = service
        .remember(&remember(&scope, "editor", "vim"))
        .expect("create");
    service
        .remember(&remember(&scope, "editor", "emacs"))
        .expect("second revision");
    service
        .remember(&remember(&scope, "editor", "helix"))
        .expect("third revision");

    let error = service
        .restore(&RestoreRequest {
            reference: reference_of(&scope, &created),
            revision: 1,
            expected_revision: 2,
        })
        .expect_err("current revision is 3");
    assert!(error.is(ErrorKind::Conflict), "error = {error}");
}
