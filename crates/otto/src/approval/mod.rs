//! Durable approval authority and domain persistence port.
pub mod config;
mod contracts;
pub mod memory;
pub mod reflection;
pub mod repository;
mod service;
pub mod workflow;
pub use contracts::*;
pub use service::{Service, digest};

// Compatibility constructors; runtime composition supplies ports to services.
impl Service {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Self::from_store(std::sync::Arc::new(
            crate::storage::backends::turso::approval::Store::open(path)?,
        ))
    }
    pub fn in_memory() -> Result<Self> {
        Self::from_store(std::sync::Arc::new(
            crate::storage::backends::turso::approval::Store::in_memory()?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Connection;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn auto_mode_is_default_scoped_persistent_and_records_both_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        let scope = proposal().scope;
        assert!(service.auto_mode(&scope).unwrap());
        let request = service.create(proposal()).unwrap();
        assert_eq!(request.state, State::Approved);
        assert_eq!(request.decision.as_ref().unwrap().transport, "auto-mode");
        let events = service.events("/w", Some("s"), 0).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].request.state, State::Pending);
        assert_eq!(events[1].request.state, State::Approved);
        service.set_auto_mode(&scope, false).unwrap();
        let mut manual = proposal();
        manual.subject.id = "manual".into();
        let manual = service.create(manual).unwrap();
        assert_eq!(manual.state, State::Pending);
        let mut other = proposal();
        other.scope.session_id = "other".into();
        assert_eq!(service.create(other).unwrap().state, State::Approved);
        let mut child = proposal();
        child.scope.task_id = "child".into();
        assert_eq!(service.create(child).unwrap().state, State::Pending);
        let mut workspace = proposal();
        workspace.scope.session_id.clear();
        assert_eq!(service.create(workspace).unwrap().state, State::Pending);
        drop(service);
        let service = Service::open(&path).unwrap();
        assert!(!service.auto_mode(&scope).unwrap());
        service.set_auto_mode(&scope, true).unwrap();
        assert_eq!(
            service.get(&manual.id, "/w", Some("s")).unwrap().state,
            State::Approved
        );
        assert_eq!(service.snapshot("/w", Some("s")).unwrap().requests.len(), 2);
    }

    #[tokio::test]
    async fn cancellation_wins_over_enabling_auto_for_a_waiter() {
        let service = Service::in_memory().unwrap();
        let scope = proposal().scope;
        service.set_auto_mode(&scope, false).unwrap();
        let request = service.create(proposal()).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = service.wait(&request.id, &scope, &cancel).await.unwrap();
        assert_eq!(result.state, State::Withdrawn);
        service.set_auto_mode(&scope, true).unwrap();
        assert_eq!(
            service.get(&request.id, "/w", Some("s")).unwrap().state,
            State::Withdrawn
        );
    }

    fn proposal() -> Proposal {
        Proposal {
            kind: Kind::Confirmation,
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
    #[test]
    fn future_database_version_is_rejected_without_modifying_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version=3").unwrap();
        assert!(matches!(Service::open(&path), Err(Error::Invalid(_))));
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
    }
    #[test]
    fn decisions_deduplicate_conflict_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let service = Service::open(&path).unwrap();
        service
            .set_auto_mode(&self::proposal().scope, false)
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
            .set_auto_mode(&self::proposal().scope, false)
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
            .set_auto_mode(&self::proposal().scope, false)
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
            CREATE TABLE approval_claims (approval_id TEXT PRIMARY KEY, owner TEXT NOT NULL, pid INTEGER NOT NULL, started TEXT NOT NULL) STRICT;
            INSERT INTO approval_claims SELECT approval_id,owner,2147483647,'2000-01-01T00:00:00Z' FROM newer_claims;
            DROP TABLE newer_claims; PRAGMA user_version=1;").unwrap();
        drop(connection);
        let service = Service::open(&path).unwrap();
        service
            .set_auto_mode(&self::proposal().scope, false)
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
        assert_eq!(version, 2);
    }
    #[test]
    fn native_review_claim_is_committed_with_the_decision_before_maintenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.db");
        let one = Service::open(&path).unwrap();
        one.set_auto_mode(&self::proposal().scope, false).unwrap();
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
        one.set_auto_mode(&self::proposal().scope, false).unwrap();
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
        one.set_auto_mode(&self::proposal().scope, false).unwrap();
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
