use super::*;
use crate::memory::{CopyStatus, RESTORED_FROM_REVISION_METADATA, StoreRestoreRequest};

fn update(
    store: &FileStore,
    scope: &Scope,
    id: &str,
    revision: u64,
    text: &str,
    second: u32,
) -> Record {
    let mut next = store.get(&record_ref(scope, id)).expect("current record");
    assert_eq!(next.revision, revision);
    next.text = text.into();
    next.updated_at = at(second);
    store
        .upsert(&UpsertRequest {
            record: next,
            expected_revision: Some(revision),
        })
        .expect("update")
}

fn history_file(store: &FileStore, id: &str, name: &str) -> PathBuf {
    store.root().join("history").join(id).join(name)
}

fn restore_request(scope: &Scope, id: &str, revision: u64, expected: u64) -> StoreRestoreRequest {
    StoreRestoreRequest {
        reference: record_ref(scope, id),
        revision,
        expected_revision: expected,
        restored_at: at(30),
    }
}

/// A record at revision 3 with distinct text per revision.
fn three_revisions(store: &FileStore, scope: &Scope) {
    put(store, "rec-1", scope, "tone", "first text");
    update(store, scope, "rec-1", 1, "second text", 2);
    update(store, scope, "rec-1", 2, "third text", 3);
}

#[test]
fn an_update_copies_the_superseded_file_into_history_and_history_lists_all_revisions() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "first text");
    let revision_one_bytes = fs::read(record_path(&store, "rec-1")).unwrap();
    update(&store, &scope, "rec-1", 1, "second text", 2);
    assert_eq!(
        fs::read(history_file(&store, "rec-1", "1.md")).unwrap(),
        revision_one_bytes
    );
    assert!(
        fs::read_to_string(record_path(&store, "rec-1"))
            .unwrap()
            .contains("second text")
    );
    let revision_two_bytes = fs::read(record_path(&store, "rec-1")).unwrap();
    update(&store, &scope, "rec-1", 2, "third text", 3);
    assert_eq!(
        fs::read(history_file(&store, "rec-1", "2.md")).unwrap(),
        revision_two_bytes
    );
    assert_eq!(
        fs::metadata(store.root().join("history/rec-1"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let history = store.history(&record_ref(&scope, "rec-1")).unwrap();
    let revisions: Vec<u64> = history
        .revisions
        .iter()
        .map(|record| record.revision)
        .collect();
    assert_eq!(revisions, vec![1, 2, 3]);
    assert_eq!(history.revisions[0].text, "first text");
    assert_eq!(history.revisions[2].text, "third text");
    assert!(history.skipped.is_empty());
}

#[test]
fn restore_writes_a_new_revision_with_the_old_content_and_keeps_the_superseded_one() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    three_revisions(&store, &scope);
    let first = store
        .history(&record_ref(&scope, "rec-1"))
        .unwrap()
        .revisions[0]
        .clone();
    let restored = store
        .restore(&restore_request(&scope, "rec-1", 1, 3))
        .unwrap();
    assert_eq!(restored.revision, 4);
    assert_eq!(restored.text, first.text);
    assert_eq!(restored.key, first.key);
    assert_eq!(restored.labels, first.labels);
    assert_eq!(restored.confidence, first.confidence);
    assert_eq!(restored.expires_at, first.expires_at);
    assert_eq!(restored.created_at, first.created_at);
    assert_eq!(restored.updated_at, at(30));
    assert_eq!(
        restored
            .metadata
            .get(RESTORED_FROM_REVISION_METADATA)
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(restored.source.origin, Some(Origin::Human));
    assert_eq!(store.get(&record_ref(&scope, "rec-1")).unwrap(), restored);
    assert!(history_file(&store, "rec-1", "3.md").is_file());
    let revisions: Vec<u64> = store
        .history(&record_ref(&scope, "rec-1"))
        .unwrap()
        .revisions
        .iter()
        .map(|record| record.revision)
        .collect();
    assert_eq!(revisions, vec![1, 2, 3, 4]);
}

#[test]
fn restore_rejects_stale_missing_forgotten_and_non_older_revisions_without_changing_files() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    three_revisions(&store, &scope);
    let live_before = fs::read(record_path(&store, "rec-1")).unwrap();

    let stale = store
        .restore(&restore_request(&scope, "rec-1", 1, 2))
        .unwrap_err();
    assert_eq!(stale.kind, ErrorKind::Conflict);
    let missing = store
        .restore(&restore_request(&scope, "rec-1", 0, 3))
        .unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound);
    let current = store
        .restore(&restore_request(&scope, "rec-1", 3, 3))
        .unwrap_err();
    assert_eq!(current.kind, ErrorKind::InvalidRequest);
    assert_eq!(fs::read(record_path(&store, "rec-1")).unwrap(), live_before);
    assert!(!history_file(&store, "rec-1", "3.md").exists());

    store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 3,
            forgotten_at: at(40),
        })
        .unwrap();
    let forgotten = store
        .restore(&restore_request(&scope, "rec-1", 1, 3))
        .unwrap_err();
    assert_eq!(forgotten.kind, ErrorKind::Conflict);
}

#[test]
fn restore_that_collides_with_another_records_key_is_a_conflict() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "first text");
    let mut renamed = store.get(&record_ref(&scope, "rec-1")).unwrap();
    renamed.key = "other".into();
    renamed.updated_at = at(2);
    store
        .upsert(&UpsertRequest {
            record: renamed,
            expected_revision: Some(1),
        })
        .unwrap();
    put(&store, "rec-2", &scope, "tone", "second record");
    // Revision 1 of rec-1 holds key "tone", which rec-2 now owns.
    let error = store
        .restore(&restore_request(&scope, "rec-1", 1, 2))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(store.get(&record_ref(&scope, "rec-1")).unwrap().revision, 2);
}

#[test]
fn forget_removes_history_and_rejects_pending_candidates_that_target_the_record() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    three_revisions(&store, &scope);
    put(&store, "rec-2", &scope, "other", "unrelated");
    let mut targeting = proposal(&scope, "cand-1", CandidateAction::Update, "rec-1", 3);
    targeting.proposed.text = "proposed replacement".into();
    let mut unrelated = proposal(&scope, "cand-2", CandidateAction::Update, "rec-2", 1);
    unrelated.proposed.key = "other".into();
    store.propose(&[targeting, unrelated]).unwrap();

    let outcome = store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 3,
            forgotten_at: at(50),
        })
        .unwrap();
    assert!(!record_path(&store, "rec-1").exists());
    assert!(!store.root().join("history/rec-1").exists());
    assert_eq!(
        store.get_tombstone(&record_ref(&scope, "rec-1")).unwrap(),
        outcome.tombstone
    );

    let entries: Vec<(String, CopyStatus, String)> = outcome
        .cleanup
        .iter()
        .map(|entry| (entry.copy.clone(), entry.status, entry.detail.clone()))
        .collect();
    assert_eq!(
        entries,
        vec![
            (
                "records/rec-1.md".to_string(),
                CopyStatus::Removed,
                "record file removed".to_string()
            ),
            (
                "history/rec-1/".to_string(),
                CopyStatus::Removed,
                "2 revision files removed".to_string()
            ),
            (
                "candidates/cand-1.json".to_string(),
                CopyStatus::Removed,
                "pending update proposal rejected, text cleared, receipt archived".to_string()
            ),
        ]
    );

    assert!(!store.path("candidates", "cand-1", "json").exists());
    let receipt = store
        .get_candidate(&candidate_ref(&scope, "cand-1"))
        .unwrap();
    assert_eq!(receipt.state, CandidateState::Rejected);
    assert_eq!(receipt.decision_source, Some(Origin::Human));
    assert_eq!(receipt.decided_at, Some(at(50)));
    assert!(receipt.proposed.text.is_empty());
    assert!(store.path("candidates/archive", "cand-1", "json").is_file());
    let untouched = store
        .get_candidate(&candidate_ref(&scope, "cand-2"))
        .unwrap();
    assert_eq!(untouched.state, CandidateState::Pending);
    assert!(store.path("candidates", "cand-2", "json").is_file());
}

#[test]
fn forgetting_a_record_without_history_reports_no_history_files() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    put(&store, "rec-1", &scope, "tone", "only text");
    let outcome = store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 1,
            forgotten_at: at(5),
        })
        .unwrap();
    assert_eq!(outcome.cleanup.len(), 2);
    assert_eq!(outcome.cleanup[1].detail, "no history files");
}

#[test]
fn open_removes_history_that_survives_next_to_a_tombstone() {
    let (dir, store) = open_temp();
    let scope = user(&store);
    three_revisions(&store, &scope);
    store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 3,
            forgotten_at: at(50),
        })
        .unwrap();
    let leftover = store.root().join("history/rec-1");
    fs::create_dir_all(&leftover).unwrap();
    fs::write(leftover.join("1.md"), "leftover").unwrap();
    let root = store.root().to_path_buf();
    drop(store);
    let reopened = FileStore::open(&root, Default::default()).unwrap();
    assert!(!leftover.exists());
    assert!(reopened.get_tombstone(&record_ref(&scope, "rec-1")).is_ok());
    drop(dir);
}

#[test]
fn open_tombstones_a_record_whose_live_file_was_deleted_by_hand() {
    let (dir, store) = open_temp();
    let scope = user(&store);
    let created = put(&store, "rec-1", &scope, "tone", "first text");
    update(&store, &scope, "rec-1", 1, "second text", 2);
    fs::remove_file(record_path(&store, "rec-1")).unwrap();
    let root = store.root().to_path_buf();
    drop(store);
    let reopened = FileStore::open(&root, Default::default()).unwrap();
    assert_eq!(
        reopened.get(&record_ref(&scope, "rec-1")).unwrap_err().kind,
        ErrorKind::NotFound
    );
    let tombstone = reopened
        .get_tombstone(&record_ref(&scope, "rec-1"))
        .unwrap();
    assert_eq!(tombstone.revision, 3);
    assert_eq!(tombstone.scope, scope);
    assert_eq!(tombstone.created_at, created.created_at);
    assert!(!root.join("history/rec-1").exists());
    drop(dir);
}

#[test]
fn open_leaves_an_unparseable_orphan_history_directory_alone() {
    let (dir, store) = open_temp();
    let orphan = store.root().join("history/orphan");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("1.md"), "not a record").unwrap();
    let root = store.root().to_path_buf();
    drop(store);
    let reopened = FileStore::open(&root, Default::default()).unwrap();
    assert!(orphan.join("1.md").is_file());
    assert_eq!(
        reopened
            .get_tombstone(&record_ref(&user(&reopened), "orphan"))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    drop(dir);
}

#[test]
fn history_lists_unreadable_and_misnamed_files_as_skipped() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    three_revisions(&store, &scope);
    let history_dir = store.root().join("history/rec-1");
    fs::write(history_dir.join("7.md"), [0xff, 0xfe, 0x00]).unwrap();
    fs::write(history_dir.join("8.md"), "not a record").unwrap();
    // A valid file stored under the wrong revision number.
    fs::copy(history_dir.join("2.md"), history_dir.join("9.md")).unwrap();

    let history = store.history(&record_ref(&scope, "rec-1")).unwrap();
    let revisions: Vec<u64> = history
        .revisions
        .iter()
        .map(|record| record.revision)
        .collect();
    assert_eq!(revisions, vec![1, 2, 3]);
    let skipped: Vec<(String, String)> = history
        .skipped
        .iter()
        .map(|item| (item.file.clone(), item.reason.clone()))
        .collect();
    assert_eq!(skipped.len(), 3);
    assert_eq!(
        skipped[0],
        (
            "history/rec-1/7.md".to_string(),
            "file is not UTF-8".to_string()
        )
    );
    assert_eq!(skipped[1].0, "history/rec-1/8.md");
    assert!(!skipped[1].1.contains("not a record"));
    assert_eq!(
        skipped[2],
        (
            "history/rec-1/9.md".to_string(),
            "revision differs from file name".to_string()
        )
    );
}

fn files_under(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, found);
        } else {
            found.push(path);
        }
    }
}

#[test]
fn forget_leaves_no_store_file_containing_the_record_text() {
    let (_dir, store) = open_temp();
    let scope = user(&store);
    let marker = "zq-marker-7f3a91";
    put(&store, "rec-1", &scope, "tone", &format!("first {marker}"));
    update(&store, &scope, "rec-1", 1, &format!("second {marker}"), 2);
    let mut targeting = proposal(&scope, "cand-1", CandidateAction::Update, "rec-1", 2);
    targeting.proposed.text = format!("proposed {marker}");
    store.propose(&[targeting]).unwrap();
    let mut before = Vec::new();
    files_under(store.root(), &mut before);
    let holders = before
        .iter()
        .filter(|path| fs::read_to_string(path).is_ok_and(|text| text.contains(marker)))
        .count();
    assert_eq!(
        holders, 3,
        "record file, history file and pending candidate hold the text before forget"
    );

    store
        .forget(&StoreForgetRequest {
            reference: record_ref(&scope, "rec-1"),
            expected_revision: 2,
            forgotten_at: at(60),
        })
        .unwrap();
    let mut after = Vec::new();
    files_under(store.root(), &mut after);
    for path in after {
        let bytes = fs::read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(marker),
            "{} still holds the record text",
            path.display()
        );
    }
}
