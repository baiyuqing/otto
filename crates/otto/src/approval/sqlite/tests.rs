use super::*;
use crate::approval::{Actor, Proposal, Service};
use otto_core::approval::ApprovalMode;
use std::sync::Arc;

fn fixture_proposal() -> Proposal {
    Proposal {
        kind: Kind::Authorization,
        authority: Authority::Boundary,
        scope: Scope {
            workspace: "/backup".into(),
            session_id: "root".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        subject: Subject {
            source: "sandbox".into(),
            id: "operation".into(),
            version: "1".into(),
            tool_call_id: "call".into(),
            details: "exact command".into(),
        },
        title: "Review fixture".into(),
        reason: "scope".into(),
        persistent: false,
        blocking: true,
        expires_at: None,
    }
}
#[test]
fn backup_rejects_unrecognized_source_schema_without_publishing() {
    for alteration in [
        "ALTER TABLE approvals ADD COLUMN surprise TEXT DEFAULT (lower('UNTRUSTED'))",
        "CREATE INDEX unexpected ON approvals(lower(body))",
        "CREATE TRIGGER unexpected AFTER INSERT ON approvals BEGIN DELETE FROM approvals; END",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        drop(service);
        let mut connection = Connection::open(&path).unwrap();
        connection.execute_batch(alteration).unwrap();
        connection.execute_batch("PRAGMA user_version=3").unwrap();
        let transaction = connection.transaction().unwrap();
        assert!(backup_before_v4(&SqliteDriver, &path, &transaction, 3).is_err());
        assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("pre-v4")
        }));
        drop(transaction);
    }
}

#[test]
fn backup_and_migration_share_the_same_source_writer_fence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approvals.db");
    let service = Service::open(&path).unwrap();
    let scope = fixture_proposal().scope;
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    let request = service.create(fixture_proposal()).unwrap();
    drop(service);
    let mut source = Connection::open(&path).unwrap();
    source.execute_batch("PRAGMA user_version=3").unwrap();
    let writer = Connection::open(&path).unwrap();
    writer.busy_timeout(Duration::ZERO).unwrap();
    let tx = source.transaction().unwrap();
    backup_before_v4(&SqliteDriver, &path, &tx, 3).unwrap();
    assert!(matches!(
        writer.execute(
            "INSERT INTO approval_modes VALUES ('/backup','late','manual')",
            ()
        ),
        Err(crate::storage::Error::Busy(_))
    ));
    initialize_tx(&tx).unwrap();
    assert!(matches!(
        writer.execute(
            "INSERT INTO approval_modes VALUES ('/backup','late','manual')",
            ()
        ),
        Err(crate::storage::Error::Busy(_))
    ));
    tx.commit().unwrap();
    let backup = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("approvals.db.pre-v4-")
                && p.extension().is_some_and(|e| e == "db")
        })
        .unwrap();
    let snapshot = Connection::open_read_only(&backup).unwrap();
    assert_eq!(
        snapshot
            .query_row("PRAGMA user_version", (), |r| r.get::<i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        snapshot
            .query_row("SELECT id FROM approvals", (), |r| r.get::<String>(0))
            .unwrap(),
        request.id
    );
    assert_eq!(
        source
            .query_row("PRAGMA user_version", (), |r| r.get::<i64>(0))
            .unwrap(),
        4
    );
}
#[test]
fn expired_review_tokens_cannot_commit_or_release_a_replacement_lease() {
    let store = Arc::new(Store::in_memory().unwrap());
    let service = Service::from_store(store.clone()).unwrap();
    let scope = fixture_proposal().scope;
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    let expected = service.create(fixture_proposal()).unwrap();
    let (pid, started) = otto_platform::process_identity::current_process();
    let old = ReviewLease {
        owner: "process:old-review".into(),
        pid,
        started: started.clone(),
        expires_at: chrono::Utc::now().timestamp() - 1,
    };
    let replacement = ReviewLease {
        owner: "process:new-review".into(),
        pid,
        started,
        expires_at: chrono::Utc::now().timestamp() + 30,
    };
    assert!(store.take_review_lease(&expected, &old).unwrap());
    assert!(store.take_review_lease(&expected, &replacement).unwrap());
    store
        .release_review_lease(&expected.id, &old.owner)
        .unwrap();
    let actual: String = store
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT owner FROM approval_review_leases WHERE approval_id=?1",
            [expected.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actual, replacement.owner);
    service.set_mode(&scope, ApprovalMode::Yolo).unwrap();
    let current = store.load(&expected.id).unwrap();
    // Even an otherwise unchanged reviewer result cannot use an old token.
    assert!(matches!(
        store.commit_review(&current, &current, &old.owner),
        Err(Error::Conflict)
    ));
    store
        .release_review_lease(&expected.id, &replacement.owner)
        .unwrap();
}
#[test]
fn v3_backup_contains_complete_opaque_rows_and_application_claims() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approvals.db");
    let service = Service::open(&path).unwrap();
    let scope = fixture_proposal().scope;
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    let request = service.create(fixture_proposal()).unwrap();
    service
        .decide_and_claim(
            &request.id,
            &scope,
            request.revision,
            true,
            &Actor::local("fixture"),
        )
        .unwrap();
    drop(service);
    let connection = Connection::open(&path).unwrap();
    let body: String = connection
        .query_row(
            "SELECT body FROM approvals WHERE id=?1",
            [request.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    let opaque = body.strip_suffix('}').unwrap().to_owned()
        + ",\"future_review_extension\":{\"n\":9007199254740993}}";
    connection
        .execute(
            "UPDATE approvals SET body=?2 WHERE id=?1",
            params![request.id, opaque],
        )
        .unwrap();
    connection.execute_batch("PRAGMA user_version=3").unwrap();
    connection.close().unwrap();
    let upgraded = Service::open(&path).unwrap();
    assert_eq!(
        upgraded
            .get(&request.id, &scope.workspace, Some(&scope.session_id))
            .unwrap()
            .application,
        ApplicationState::Applying
    );
    let backup = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("approvals.db.pre-v4-")
                && p.extension().is_some_and(|v| v == "db")
        })
        .unwrap();
    assert_eq!(
        std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let snapshot = Connection::open_read_only(&backup).unwrap();
    assert_eq!(
        snapshot
            .query_row("PRAGMA user_version", (), |r| r.get::<i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        snapshot
            .query_row(
                "SELECT body FROM approvals WHERE id=?1",
                [request.id.as_str()],
                |r| r.get::<String>(0)
            )
            .unwrap(),
        opaque
    );
    assert_eq!(
        snapshot
            .query_row("SELECT COUNT(*) FROM approval_claims", (), |r| r
                .get::<i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        snapshot
            .query_row("SELECT COUNT(*) FROM approval_events", (), |r| r
                .get::<i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        snapshot
            .query_row("SELECT COUNT(*) FROM approval_visibility", (), |r| r
                .get::<i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn stale_snapshot_or_auto_mode_never_publishes_a_partial_decision() {
    let store = Arc::new(Store::in_memory().unwrap());
    let service = Service::from_store(store.clone()).unwrap();
    let scope = Scope {
        workspace: "/w".into(),
        session_id: "s".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    let pending = service
        .create(Proposal {
            kind: Kind::Authorization,
            authority: Authority::Bounded,
            scope: scope.clone(),
            subject: Subject {
                tool_call_id: String::new(),
                source: "plugin".into(),
                id: "plugin".into(),
                version: "1".into(),
                details: "review".into(),
            },
            title: "Review".into(),
            reason: "scope".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    let mut next = pending.clone();
    next.state = State::Approved;
    assert!(matches!(
        store.commit(&pending, &next, ClaimChange::None, true),
        Err(Error::Conflict)
    ));
    assert_eq!(store.load(&pending.id).unwrap(), pending);
    assert_eq!(store.events("/w", Some("s"), 0).unwrap().len(), 1);
    service
        .decide(&pending.id, &scope, 1, false, "", &Actor::local("test"))
        .unwrap();
    assert!(matches!(
        store.commit(&pending, &next, ClaimChange::None, false),
        Err(Error::Conflict)
    ));
    assert_eq!(store.load(&pending.id).unwrap().state, State::Rejected);
    assert_eq!(store.events("/w", Some("s"), 0).unwrap().len(), 2);
}

#[test]
fn the_commit_fence_rechecks_the_persisted_mode_for_yolo_and_manual() {
    let store = Arc::new(Store::in_memory().unwrap());
    let service = Service::from_store(store.clone()).unwrap();
    let scope = Scope {
        workspace: "/w".into(),
        session_id: "s".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    // A consent request: the class AUTO may never decide.
    let review = Proposal {
        kind: Kind::Confirmation,
        authority: Authority::Consent,
        scope: scope.clone(),
        subject: Subject {
            tool_call_id: String::new(),
            source: "conversation".into(),
            id: "plan".into(),
            version: "1".into(),
            details: "review".into(),
        },
        title: "Review".into(),
        reason: "scope".into(),
        persistent: false,
        blocking: true,
        expires_at: None,
    };
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    let pending = service.create(review).unwrap();
    assert_eq!(pending.state, State::Pending);
    let mut next = pending.clone();
    next.state = State::Approved;
    // MANUAL refuses it even though the service could compute the
    // decision, so the fence reads the persisted mode, not the caller.
    assert!(matches!(
        store.commit(&pending, &next, ClaimChange::None, true),
        Err(Error::Conflict)
    ));
    assert_eq!(store.load(&pending.id).unwrap(), pending);
    // The same commit lands once the store itself is in YOLO. The store
    // port is used so the pending request is not swept by `set_mode`:
    // the fence, not the sweep, is what publishes this decision.
    store.set_mode(&scope, ApprovalMode::Yolo).unwrap();
    next.review.as_mut().unwrap().settings_revision =
        store.settings(&scope).unwrap().unwrap().revision;
    next.review.as_mut().unwrap().status = ReviewStatus::Completed;
    next.review.as_mut().unwrap().policy_digest = crate::approval::digest(
        &serde_json::to_string(&otto_core::config::ApprovalConfig::default()).unwrap(),
    );
    next.decision = Some(Decision {
        option_id: "approve".into(),
        feedback: "Explicit bypass preset.".into(),
        actor: "approval-policy".into(),
        transport: "yolo-mode".into(),
        decided_at: chrono::Utc::now().timestamp(),
    });
    let approved = store
        .commit(&pending, &next, ClaimChange::None, true)
        .unwrap();
    assert_eq!(approved.state, State::Approved);
}

#[test]
fn a_boolean_mode_column_migrates_to_the_mode_form_with_its_meaning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approvals.db");
    {
        let connection = Connection::open(&path).unwrap();
        connection
                .execute(
                    "CREATE TABLE approval_modes (workspace TEXT NOT NULL,\
                     session_id TEXT NOT NULL, automatic INTEGER NOT NULL CHECK(automatic IN (0,1)),\
                     PRIMARY KEY(workspace,session_id)) STRICT",
                    (),
                )
                .unwrap();
        connection
            .execute(
                "INSERT INTO approval_modes VALUES (?1,?2,?3)",
                params!["/w", "off", 0],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO approval_modes VALUES (?1,?2,?3)",
                params!["/w", "on", 1],
            )
            .unwrap();
        connection.execute_batch("PRAGMA user_version=2").unwrap();
    }
    let store = match Store::open(&path) {
        Ok(store) => store,
        Err(error) => panic!("open failed: {error}"),
    };
    fn mode(store: &Store, session: &str) -> Option<ApprovalMode> {
        store
            .mode(&Scope {
                workspace: "/w".into(),
                session_id: session.into(),
                task_id: String::new(),
                run_id: String::new(),
            })
            .unwrap()
    }
    // A stored `automatic = 0` was MANUAL and `1` was AUTO; neither is
    // re-read as the default, and YOLO is reachable afterwards.
    assert_eq!(mode(&store, "off"), Some(ApprovalMode::Manual));
    assert_eq!(mode(&store, "on"), Some(ApprovalMode::Auto));
    assert_eq!(mode(&store, "absent"), None);
    store
        .set_mode(
            &Scope {
                workspace: "/w".into(),
                session_id: "on".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            ApprovalMode::Yolo,
        )
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(mode(&store, "on"), Some(ApprovalMode::Yolo));
    assert_eq!(mode(&store, "off"), Some(ApprovalMode::Manual));
    let version: i64 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", (), |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
}

#[test]
fn auto_mode_never_publishes_a_decision_for_a_request_it_cannot_decide() {
    let store = Arc::new(Store::in_memory().unwrap());
    let service = Service::from_store(store.clone()).unwrap();
    let scope = Scope {
        workspace: "/w".into(),
        session_id: "s".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    // AUTO is the default for a new session, so this fence is the only
    // thing standing between a plan review and an automatic decision.
    assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Auto);
    let pending = service
        .create(Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Consent,
            scope: scope.clone(),
            subject: Subject {
                tool_call_id: String::new(),
                source: "conversation".into(),
                id: "plan".into(),
                version: "1".into(),
                details: "review".into(),
            },
            title: "Review".into(),
            reason: "scope".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    assert_eq!(pending.state, State::Pending);
    let mut next = pending.clone();
    next.state = State::Approved;
    assert!(matches!(
        store.commit(&pending, &next, ClaimChange::None, true),
        Err(Error::Conflict)
    ));
    assert_eq!(store.load(&pending.id).unwrap(), pending);
    assert_eq!(store.events("/w", Some("s"), 0).unwrap().len(), 1);
}

#[test]
fn a_stored_managed_policy_row_with_fields_the_config_no_longer_has_still_reads() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("approvals.db")).unwrap();
    let connection = store.connection.lock().unwrap();
    // The row shape written while the row was a serialized ApprovalConfig.
    connection
        .execute(
            "INSERT INTO approval_managed_policy VALUES (?1,?2)",
            params![
                "/w",
                r#"{"policy":"never","reviewer":null,"reviewer_model":null,"reviewer_profile":null,"rules":[]}"#
            ],
        )
        .unwrap();
    let forced = read_managed(&connection, "/w").unwrap().unwrap();
    assert_eq!(forced.policy, Some(ApprovalPolicy::Never));
    assert_eq!(forced.reviewer, None);
}

#[test]
fn a_written_managed_policy_row_holds_only_policy_and_reviewer() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("approvals.db")).unwrap();
    let forced = otto_core::config::ApprovalConfig {
        policy: Some(ApprovalPolicy::Never),
        reviewer: Some(ApprovalReviewer::User),
        reviewer_model: Some("claude/review".into()),
        ..Default::default()
    };
    store.managed_policy("/w", &forced).unwrap();
    let body: String = store
        .connection
        .lock()
        .unwrap()
        .query_row(
            "SELECT body FROM approval_managed_policy WHERE workspace=?1",
            ["/w"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body, r#"{"policy":"never","reviewer":"user"}"#);
}
