//! Durable approval authority and domain persistence port.
pub mod codec;
mod contracts;
mod coordinator;
pub mod memory;
pub mod model_change;
mod policy;
pub mod repository;
pub mod reviewer;
mod service;
pub mod sqlite;
pub mod workflow;
pub use contracts::*;
pub use service::{Service, digest};
#[cfg(test)]
mod coordinator_tests;

// Compatibility constructors; runtime composition supplies ports to services.
impl Service {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Self::from_store(std::sync::Arc::new(crate::approval::sqlite::Store::open(
            path,
        )?))
    }
    pub fn in_memory() -> Result<Self> {
        Self::from_store(std::sync::Arc::new(
            crate::approval::sqlite::Store::in_memory()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Connection;
    use otto_core::approval::{
        ApplicationState, ApprovalMode, Authority, Kind, Request, Scope, State, Subject,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn auto_mode_decides_only_a_bounded_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        let scope = proposal().scope;
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Auto);
        // A model-originated plan review is not the session policy's decision.
        let review = service.create(proposal()).unwrap();
        assert_eq!(review.state, State::Pending);
        assert!(review.decision.is_none());
        assert_eq!(service.events("/w", Some("s"), 0).unwrap().len(), 1);
        // A bounded authorization is still decided without a dialog, and both
        // events are recorded.
        let mut bounded = authorization();
        bounded.subject.id = "bounded".into();
        let bounded = service.create(bounded).unwrap();
        assert_eq!(bounded.state, State::Approved);
        assert_eq!(
            bounded.decision.as_ref().unwrap().transport,
            "bounded-authority"
        );
        let events = service.events("/w", Some("s"), 0).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].request.state, State::Pending);
        assert_eq!(events[1].request.state, State::Pending);
        assert_eq!(events[2].request.state, State::Approved);
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let mut manual = authorization();
        manual.subject.id = "manual".into();
        let manual = service.create(manual).unwrap();
        assert_eq!(manual.state, State::Pending);
        let mut other = authorization();
        other.subject.id = "other".into();
        other.scope.session_id = "other".into();
        assert_eq!(service.create(other).unwrap().state, State::Approved);
        let mut child = authorization();
        child.subject.id = "child".into();
        child.scope.task_id = "child".into();
        assert_eq!(service.create(child).unwrap().state, State::Pending);
        let mut workspace = authorization();
        workspace.subject.id = "workspace".into();
        workspace.scope.session_id.clear();
        assert_eq!(service.create(workspace).unwrap().state, State::Pending);
        drop(service);
        let service = Service::open(&path).unwrap();
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
        service.set_mode(&scope, ApprovalMode::Auto).unwrap();
        // Switching to AUTO decides the pending bounded authorization and
        // leaves the plan review for the user.
        assert_eq!(
            service.get(&manual.id, "/w", Some("s")).unwrap().state,
            State::Approved
        );
        assert_eq!(
            service.get(&review.id, "/w", Some("s")).unwrap().state,
            State::Pending
        );
        // The root's live AUTO preset also covers its own bounded child.
        assert_eq!(service.snapshot("/w", Some("s")).unwrap().requests.len(), 2);
    }

    #[tokio::test]
    async fn yolo_mode_decides_every_request_owned_by_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        let scope = proposal().scope;
        service.set_mode(&scope, ApprovalMode::Yolo).unwrap();
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Yolo);
        // Every class AUTO refuses: a plan review, a boundary crossing, a
        // persistent durable grant, and a request of the session's own child
        // task. Each is decided and journalled, and the decision names YOLO.
        let review = service.create(proposal()).unwrap();
        assert_eq!(review.state, State::Approved);
        assert_eq!(review.decision.as_ref().unwrap().transport, "yolo-mode");
        let mut escalation = authorization();
        escalation.kind = Kind::Authorization;
        escalation.authority = Authority::Boundary;
        escalation.subject.id = "escalation".into();
        assert_eq!(service.create(escalation).unwrap().state, State::Approved);
        let mut grant = authorization();
        grant.authority = Authority::Durable;
        grant.persistent = true;
        grant.subject.id = "grant".into();
        assert_eq!(service.create(grant).unwrap().state, State::Approved);
        let mut child = authorization();
        child.authority = Authority::Boundary;
        child.subject.id = "child".into();
        child.scope.task_id = "child".into();
        assert_eq!(service.create(child).unwrap().state, State::Approved);
        // YOLO is still the session's policy, not the workspace's or another
        // session's: the scope check is unchanged.
        let mut workspace = authorization();
        workspace.subject.id = "workspace".into();
        workspace.scope.session_id.clear();
        assert_eq!(service.create(workspace).unwrap().state, State::Pending);
        let mut other = authorization();
        other.authority = Authority::Boundary;
        other.subject.id = "other".into();
        other.scope.session_id = "other".into();
        assert_eq!(service.create(other).unwrap().state, State::Pending);
        // The mode survives a restart, and turning it off stops deciding.
        drop(service);
        let service = Service::open(&path).unwrap();
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Yolo);
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let mut after = authorization();
        after.authority = Authority::Boundary;
        after.subject.id = "after".into();
        let after = service.create(after).unwrap();
        assert_eq!(after.state, State::Pending);
        // Switching back to YOLO approves what is waiting.
        service.set_mode(&scope, ApprovalMode::Yolo).unwrap();
        assert_eq!(
            service.get(&after.id, "/w", Some("s")).unwrap().state,
            State::Approved
        );
    }

    #[test]
    fn auto_does_not_decide_what_yolo_decides() {
        // The two policies must not be confused with each other: this fails
        // if YOLO ever becomes the default or AUTO decides a boundary.
        let service = Service::in_memory().unwrap();
        let scope = proposal().scope;
        assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Auto);
        assert_eq!(service.create(proposal()).unwrap().state, State::Pending);
        let mut escalation = authorization();
        escalation.authority = Authority::Boundary;
        escalation.subject.id = "escalation".into();
        assert_eq!(service.create(escalation).unwrap().state, State::Pending);
    }

    #[tokio::test]
    async fn cancellation_wins_over_enabling_auto_for_a_waiter() {
        let service = Service::in_memory().unwrap();
        let scope = proposal().scope;
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        let request = service.create(proposal()).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = service.wait(&request.id, &scope, &cancel).await.unwrap();
        assert_eq!(result.state, State::Withdrawn);
        service.set_mode(&scope, ApprovalMode::Auto).unwrap();
        assert_eq!(
            service.get(&request.id, "/w", Some("s")).unwrap().state,
            State::Withdrawn
        );
    }

    fn proposal() -> Proposal {
        Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Consent,
            scope: Scope {
                workspace: "/w".into(),
                session_id: "s".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            subject: Subject {
                tool_call_id: String::new(),
                source: "conversation".into(),
                id: "operation-1".into(),
                version: "1".into(),
                details: "plan".into(),
            },
            title: "Review plan".into(),
            reason: "scope".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        }
    }
    /// A bounded authorization: this is the class the session AUTO policy may
    /// decide on its own.
    fn authorization() -> Proposal {
        Proposal {
            kind: Kind::Authorization,
            authority: Authority::Bounded,
            scope: Scope {
                workspace: "/w".into(),
                session_id: "s".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            subject: Subject {
                tool_call_id: String::new(),
                source: "plugin".into(),
                id: "plugin-1".into(),
                version: "1".into(),
                details: "run a plugin command".into(),
            },
            title: "Run a plugin command".into(),
            reason: "inside the current sandbox".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        }
    }
    #[test]
    fn future_database_version_is_rejected_without_modifying_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version=5").unwrap();
        assert!(matches!(Service::open(&path), Err(Error::Invalid(_))));
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .unwrap();
        assert_eq!(version, 5);
    }
    #[test]
    fn decisions_deduplicate_conflict_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        service
            .set_mode(&self::proposal().scope, ApprovalMode::Manual)
            .unwrap();
        let r = service.create(proposal()).unwrap();
        assert_eq!(service.create(proposal()).unwrap().id, r.id);
        let snapshot = service.snapshot("/w", Some("s")).unwrap();
        assert_eq!(snapshot.requests.len(), 1);
        let approved = service
            .decide(&r.id, &r.scope, 1, true, "go", &Actor::local("test"))
            .unwrap();
        assert_eq!(approved.state, State::Approved);
        assert_eq!(
            service
                .decide(&r.id, &r.scope, 1, true, "go", &Actor::local("test"))
                .unwrap(),
            approved
        );
        assert!(matches!(
            service.decide(&r.id, &r.scope, 1, false, "", &Actor::local("test")),
            Err(Error::Conflict)
        ));
        assert!(matches!(
            service.get(&r.id, "/foreign", None),
            Err(Error::Unauthorized)
        ));
        assert_eq!(
            service
                .events("/w", Some("s"), snapshot.cursor)
                .unwrap()
                .len(),
            1
        );
        drop(service);
        let reopened = Service::open(&path).unwrap();
        assert_eq!(reopened.get(&r.id, "/w", Some("s")).unwrap(), approved);
        assert!(
            reopened
                .snapshot("/w", Some("s"))
                .unwrap()
                .requests
                .is_empty()
        );
    }
    #[tokio::test]
    async fn cancelled_wait_withdraws_without_approval() {
        let service = Service::in_memory().unwrap();
        service
            .set_mode(&self::proposal().scope, ApprovalMode::Manual)
            .unwrap();
        let r = service.create(proposal()).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            service.wait(&r.id, &r.scope, &cancel).await.unwrap().state,
            State::Withdrawn
        );
        assert!(matches!(
            service.decide(&r.id, &r.scope, 1, true, "", &Actor::local("test")),
            Err(Error::Conflict)
        ));
    }
    #[test]
    fn legacy_claims_migrate_conservatively_without_replaying_a_native_review() {
        struct NoReplay;
        impl Domain for NoReplay {
            fn details(&self, request: &Request) -> Result<String> {
                Ok(request.subject.details.clone())
            }
            fn apply(&self, _: &Request) -> Result<()> {
                panic!("interrupted native reviews must not be replayed")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        service
            .set_mode(&self::proposal().scope, ApprovalMode::Manual)
            .unwrap();
        let mut proposal = proposal();
        proposal.kind = Kind::Authorization;
        proposal.subject.source = "native-review".into();
        let request = service.create(proposal).unwrap();
        service
            .decide_and_claim(
                &request.id,
                &request.scope,
                1,
                true,
                &Actor::local("review"),
            )
            .unwrap();
        drop(service);
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("ALTER TABLE approval_claims RENAME TO newer_claims;
            CREATE TABLE approval_claims (approval_id TEXT PRIMARY KEY REFERENCES approvals(id), owner TEXT NOT NULL, pid INTEGER NOT NULL, started TEXT NOT NULL) STRICT;
            INSERT INTO approval_claims SELECT approval_id,owner,2147483647,'2000-01-01T00:00:00Z' FROM newer_claims;
            DROP TABLE newer_claims; PRAGMA user_version=1;").unwrap();
        drop(connection);
        let service = Service::open(&path).unwrap();
        service
            .set_mode(&self::proposal().scope, ApprovalMode::Manual)
            .unwrap();
        service
            .register_domain("/w", "native-review", Arc::new(NoReplay))
            .unwrap();
        service.apply_available("/w").unwrap();
        assert_eq!(
            service
                .get(&request.id, "/w", Some("s"))
                .unwrap()
                .application,
            ApplicationState::Unknown
        );
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }
    #[test]
    fn native_review_claim_is_committed_with_the_decision_before_maintenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let one = Service::open(&path).unwrap();
        one.set_mode(&self::proposal().scope, ApprovalMode::Manual)
            .unwrap();
        let other = Service::open(&path).unwrap();
        let mut proposal = proposal();
        proposal.kind = Kind::Authorization;
        proposal.subject.source = "memory".into();
        let request = one.create(proposal).unwrap();
        let decided = one
            .decide_and_claim(
                &request.id,
                &request.scope,
                1,
                true,
                &Actor::local("review"),
            )
            .unwrap();
        assert_eq!(decided.application, ApplicationState::Applying);
        other.apply_available("/w").unwrap();
        assert_eq!(
            other.get(&request.id, "/w", Some("s")).unwrap().application,
            ApplicationState::Applying
        );
        assert!(matches!(
            other.application(
                &request.id,
                &request.scope,
                ApplicationState::NotStarted,
                ApplicationState::Applying,
                ""
            ),
            Err(Error::Conflict)
        ));
        one.application(
            &request.id,
            &request.scope,
            ApplicationState::Applying,
            ApplicationState::Applied,
            "",
        )
        .unwrap();
    }
    #[test]
    fn separate_connections_have_one_decision_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let one = Service::open(&path).unwrap();
        one.set_mode(&self::proposal().scope, ApprovalMode::Manual)
            .unwrap();
        let two = Service::open(&path).unwrap();
        let r = one.create(proposal()).unwrap();
        one.decide(&r.id, &r.scope, 1, true, "", &Actor::local("one"))
            .unwrap();
        assert!(matches!(
            two.decide(&r.id, &r.scope, 1, false, "", &Actor::local("two")),
            Err(Error::Conflict)
        ));
    }
    #[test]
    fn recovery_does_not_take_over_a_live_claim_or_replay_a_lost_external_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let one = Service::open(&path).unwrap();
        one.set_mode(&self::proposal().scope, ApprovalMode::Manual)
            .unwrap();
        let two = Service::open(&path).unwrap();
        let mut p = proposal();
        p.kind = Kind::Authorization;
        p.subject.source = "sandbox".into();
        let request = one.create(p).unwrap();
        one.decide(
            &request.id,
            &request.scope,
            1,
            true,
            "",
            &Actor::local("test-human"),
        )
        .unwrap();
        one.application(
            &request.id,
            &request.scope,
            ApplicationState::NotStarted,
            ApplicationState::Applying,
            "",
        )
        .unwrap();
        two.apply_available("/w").unwrap();
        assert_eq!(
            two.get(&request.id, "/w", Some("s")).unwrap().application,
            ApplicationState::Applying
        );
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE approval_claims SET pid=2147483000 WHERE approval_id=?1",
                [request.id.as_str()],
            )
            .unwrap();
        two.apply_available("/w").unwrap();
        assert_eq!(
            two.get(&request.id, "/w", Some("s")).unwrap().application,
            ApplicationState::Unknown
        );
        assert!(matches!(
            one.application(
                &request.id,
                &request.scope,
                ApplicationState::Applying,
                ApplicationState::Applied,
                ""
            ),
            Err(Error::Conflict)
        ));
    }
}
