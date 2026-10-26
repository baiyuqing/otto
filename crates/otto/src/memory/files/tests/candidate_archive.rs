use super::*;

fn history_request(scope: &Scope, states: Vec<CandidateState>) -> CandidateListRequest {
    CandidateListRequest {
        scopes: vec![scope.clone()],
        states,
        limit: 10,
        cursor: String::new(),
    }
}

#[test]
fn review_archives_both_decisions_and_preserves_exact_lookup_and_history() {
    let (_directory, store) = open_temp();
    let scope = user(&store);
    for id in ["accepted", "rejected", "pending"] {
        store
            .propose(&[proposal(&scope, id, CandidateAction::Create, "", 0)])
            .unwrap();
    }
    let accepted = store.review(&accept(&scope, "accepted", "result")).unwrap();
    let mut reject = accept(&scope, "rejected", "");
    reject.decision = ReviewDecision::Reject;
    let rejected = store.review(&reject).unwrap();
    for outcome in [&accepted, &rejected] {
        let id = &outcome.candidate.id;
        assert!(!store.path("candidates", id, "json").exists());
        let archived = store.path("candidates/archive", id, "json");
        assert!(archived.is_file());
        assert_eq!(
            fs::metadata(archived).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            store.get_candidate(&candidate_ref(&scope, id)).unwrap(),
            outcome.candidate
        );
        let wrong_scope = Scope::new(NAMESPACE_WORKSPACE, "another-workspace");
        assert_eq!(
            store
                .get_candidate(&candidate_ref(&wrong_scope, id))
                .unwrap_err()
                .kind,
            ErrorKind::NotFound
        );
        assert_eq!(
            store
                .propose(&[proposal(&scope, id, CandidateAction::Create, "", 0)])
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
    }
    assert_eq!(store.snapshot().unwrap().candidates.len(), 1);
    let pending = store
        .list_candidates(&history_request(&scope, vec![CandidateState::Pending]))
        .unwrap();
    assert_eq!(pending.candidates.len(), 1);
    assert_eq!(pending.candidates[0].id, "pending");
    let all = store
        .list_candidates(&history_request(&scope, vec![]))
        .unwrap();
    assert_eq!(all.candidates.len(), 3);
    let accepted_history = store
        .list_candidates(&history_request(&scope, vec![CandidateState::Accepted]))
        .unwrap();
    assert_eq!(accepted_history.candidates, vec![accepted.candidate]);
    let records_before = fs::read(record_path(&store, "result")).unwrap();
    assert_eq!(
        store
            .review(&accept(&scope, "accepted", "result"))
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        fs::read(record_path(&store, "result")).unwrap(),
        records_before
    );
}

#[test]
fn opening_migrates_legacy_receipts_and_resumes_an_interrupted_archive() {
    let (_directory, store) = open_temp();
    let scope = user(&store);
    store
        .propose(&[proposal(&scope, "accepted", CandidateAction::Create, "", 0)])
        .unwrap();
    store.review(&accept(&scope, "accepted", "result")).unwrap();
    store
        .propose(&[proposal(&scope, "rejected", CandidateAction::Create, "", 0)])
        .unwrap();
    let mut reject = accept(&scope, "rejected", "");
    reject.decision = ReviewDecision::Reject;
    store.review(&reject).unwrap();
    // A legacy receipt exists only in the active directory.
    fs::rename(
        store.path("candidates/archive", "accepted", "json"),
        store.path("candidates", "accepted", "json"),
    )
    .unwrap();
    // An interrupted archive leaves identical receipts in both directories.
    fs::copy(
        store.path("candidates/archive", "rejected", "json"),
        store.path("candidates", "rejected", "json"),
    )
    .unwrap();
    store
        .propose(&[proposal(&scope, "pending", CandidateAction::Create, "", 0)])
        .unwrap();
    let pending_before = fs::read(store.path("candidates", "pending", "json")).unwrap();
    let records_before = fs::read(record_path(&store, "result")).unwrap();
    fs::write(store.path("candidates", "invalid", "json"), "invalid JSON").unwrap();
    let root = store.root().to_path_buf();
    store.close().unwrap();
    for _ in 0..2 {
        let reopened = FileStore::open(&root, MemoryOptions::default()).unwrap();
        for id in ["accepted", "rejected"] {
            assert!(!reopened.path("candidates", id, "json").exists());
            assert!(reopened.get_candidate(&candidate_ref(&scope, id)).is_ok());
        }
        assert_eq!(
            fs::read(reopened.path("candidates", "pending", "json")).unwrap(),
            pending_before
        );
        assert_eq!(
            fs::read(record_path(&reopened, "result")).unwrap(),
            records_before
        );
        assert_eq!(
            fs::read(reopened.path("candidates", "invalid", "json")).unwrap(),
            b"invalid JSON"
        );
        assert_eq!(reopened.invalid_files().unwrap().len(), 1);
        reopened.close().unwrap();
    }
}

#[test]
fn ordinary_reads_do_not_scan_the_archive_and_explicit_history_pages_it() {
    let (_directory, store) = open_temp();
    let scope = user(&store);
    for id in ["first", "second", "third"] {
        store
            .propose(&[proposal(&scope, id, CandidateAction::Create, "", 0)])
            .unwrap();
        let mut reject = accept(&scope, id, "");
        reject.decision = ReviewDecision::Reject;
        store.review(&reject).unwrap();
    }
    fs::write(
        store.path("candidates/archive", "invalid", "json"),
        "invalid JSON",
    )
    .unwrap();
    assert!(store.invalid_files().unwrap().is_empty());
    assert!(
        store
            .list_candidates(&history_request(&scope, vec![CandidateState::Pending]))
            .unwrap()
            .candidates
            .is_empty()
    );
    let mut request = history_request(&scope, vec![CandidateState::Rejected]);
    request.limit = 1;
    let mut ids = Vec::new();
    loop {
        let page = store.list_candidates(&request).unwrap();
        ids.extend(page.candidates.into_iter().map(|candidate| candidate.id));
        if page.next_cursor.is_empty() {
            break;
        }
        request.cursor = page.next_cursor;
    }
    assert_eq!(ids, ["first", "second", "third"]);
    assert_eq!(
        store
            .get_candidate(&candidate_ref(&scope, "invalid"))
            .unwrap_err()
            .kind,
        ErrorKind::Corrupt
    );
}

#[test]
fn archived_receipts_cannot_be_symlinks_or_pending_proposals() {
    let (_directory, store) = open_temp();
    let scope = user(&store);
    let pending = proposal(&scope, "pending", CandidateAction::Create, "", 0);
    fs::write(
        store.path("candidates/archive", "pending", "json"),
        format::encode_candidate_file(&pending).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store
            .get_candidate(&candidate_ref(&scope, "pending"))
            .unwrap_err()
            .kind,
        ErrorKind::Corrupt
    );
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        store.path("candidates/archive", "linked", "json"),
    )
    .unwrap();
    assert_eq!(
        store
            .get_candidate(&candidate_ref(&scope, "linked"))
            .unwrap_err()
            .kind,
        ErrorKind::Corrupt
    );
}

#[test]
fn migration_never_overwrites_a_conflicting_archived_receipt() {
    let (_directory, store) = open_temp();
    let scope = user(&store);
    store
        .propose(&[proposal(&scope, "accepted", CandidateAction::Create, "", 0)])
        .unwrap();
    let outcome = store.review(&accept(&scope, "accepted", "result")).unwrap();
    let archived_path = store.path("candidates/archive", "accepted", "json");
    let archived_before = fs::read(&archived_path).unwrap();
    let mut conflicting = outcome.candidate;
    conflicting.decided_at = Some(at(3));
    let active_path = store.path("candidates", "accepted", "json");
    let active_before = format::encode_candidate_file(&conflicting).unwrap();
    fs::write(&active_path, &active_before).unwrap();
    let records_before = fs::read(record_path(&store, "result")).unwrap();
    let error = FileStore::open(store.root(), MemoryOptions::default())
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(fs::read(archived_path).unwrap(), archived_before);
    assert_eq!(fs::read_to_string(active_path).unwrap(), active_before);
    assert_eq!(
        fs::read(record_path(&store, "result")).unwrap(),
        records_before
    );
}

#[test]
fn an_archive_directory_symlink_is_refused_on_open() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("memory");
    fs::create_dir_all(root.join("candidates")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("candidates/archive")).unwrap();
    assert!(FileStore::open(&root, MemoryOptions::default()).is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
