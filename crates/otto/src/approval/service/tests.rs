use super::*;
use otto_core::approval::{Authority, Subject};
use std::sync::Mutex;

struct TestStore {
    request: Mutex<Option<Request>>,
    events: Mutex<Vec<Change>>,
}
impl ApprovalStore for TestStore {
    fn mode(&self, _: &Scope) -> Result<Option<ApprovalMode>> {
        Ok(Some(ApprovalMode::Manual))
    }
    fn set_mode(&self, _: &Scope, _: ApprovalMode) -> Result<()> {
        Ok(())
    }
    fn create(&self, _: &str, request: &Request) -> Result<(Request, bool)> {
        *self.request.lock().unwrap() = Some(request.clone());
        Ok((request.clone(), true))
    }
    fn load(&self, _: &str) -> Result<Request> {
        self.request.lock().unwrap().clone().ok_or(Error::NotFound)
    }
    fn visible(&self, _: &str, workspace: &str, _: Option<&str>) -> Result<bool> {
        Ok(workspace == "/w")
    }
    fn pending(&self) -> Result<Vec<Request>> {
        Ok(vec![])
    }
    fn applications(&self, _: &str) -> Result<Vec<Request>> {
        Ok(vec![])
    }
    fn snapshot(&self, _: &str, _: Option<&str>) -> Result<Snapshot> {
        Ok(Snapshot {
            requests: vec![],
            cursor: 0,
        })
    }
    fn events(&self, _: &str, _: Option<&str>, _: i64) -> Result<Vec<Change>> {
        Ok(self.events.lock().unwrap().clone())
    }
    fn claim(&self, _: &str) -> Result<Option<Claim>> {
        Ok(None)
    }
    fn take_claim(&self, _: &str, _: &Claim, _: &Claim) -> Result<bool> {
        Ok(false)
    }
    fn commit(
        &self,
        expected: &Request,
        next: &Request,
        _: ClaimChange,
        _: bool,
    ) -> Result<Request> {
        let mut current = self.request.lock().unwrap();
        if current.as_ref() != Some(expected) {
            return Err(Error::Conflict);
        }
        *current = Some(next.clone());
        self.events.lock().unwrap().push(Change {
            sequence: 1,
            request: next.clone(),
        });
        Ok(next.clone())
    }
}

#[test]
fn injected_store_keeps_revision_and_human_authority_in_the_service() {
    let store = Arc::new(TestStore {
        request: Mutex::new(None),
        events: Mutex::new(vec![]),
    });
    let service = Service::from_store(store.clone()).unwrap();
    let request = service
        .create(Proposal {
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
    let actor = Actor::local("test");
    assert!(matches!(
        service.decide(&request.id, &request.scope, 2, true, "", &actor),
        Err(Error::Conflict)
    ));
    let mut foreign = request.scope.clone();
    foreign.workspace = "/foreign".into();
    assert!(matches!(
        service.decide(&request.id, &foreign, 1, true, "", &actor),
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        service.decide(
            &request.id,
            &request.scope,
            1,
            true,
            "",
            &Actor {
                identity: String::new(),
                transport: "model".into()
            }
        ),
        Err(Error::Invalid(_))
    ));
    assert!(store.events.lock().unwrap().is_empty());
    let approved = service
        .decide(&request.id, &request.scope, 1, true, "", &actor)
        .unwrap();
    assert_eq!(approved.state, State::Approved);
    assert_eq!(approved.decision.unwrap().actor, "local-user");
    assert_eq!(store.events.lock().unwrap().len(), 1);
}
