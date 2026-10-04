//! Workflow owns the transactional request/step/result receipt and DAG resume.
use super::{Domain, Error, Kind, Proposal, Request, Result, Service, State, Subject};
use std::sync::{Arc, Weak};
pub struct Adapter {
    pub controller: Weak<crate::workflow::Controller>,
}
impl Domain for Adapter {
    fn details(&self, request: &Request) -> Result<String> {
        let ctrl = self.controller.upgrade().ok_or(Error::Unavailable)?;
        let pending = ctrl
            .approval_request(&request.subject.id)
            .map_err(|_| Error::Unavailable)?;
        if pending.run_id != request.scope.run_id {
            return Err(Error::Unauthorized);
        }
        Ok(pending.prompt)
    }
    fn apply(&self, request: &Request) -> Result<()> {
        let ctrl = self.controller.upgrade().ok_or(Error::Unavailable)?;
        let pending = ctrl
            .approval_request(&request.subject.id)
            .map_err(|_| Error::Unavailable)?;
        if pending.run_id != request.scope.run_id
            || super::digest(&pending.prompt) != request.subject.version
        {
            return Err(Error::Conflict);
        }
        ctrl.respond_authorized(&pending.id, request.state == State::Approved)
            .map(|_| ())
            .map_err(|_| Error::Conflict)
    }
}
pub fn publish(
    service: &Service,
    workspace: &str,
    request: &crate::workflow::ApprovalRequest,
) -> Result<Request> {
    service.create(Proposal {
        kind: Kind::Authorization,
        scope: super::Scope {
            workspace: workspace.into(),
            session_id: String::new(),
            task_id: String::new(),
            run_id: request.run_id.clone(),
        },
        subject: Subject {
            tool_call_id: String::new(),
            source: "workflow".into(),
            id: request.id.clone(),
            version: super::digest(&request.prompt),
            details: request.prompt.clone(),
        },
        title: format!("Approve workflow step {}?", request.step_id),
        reason: "Continue the dependent workflow steps.".into(),
        persistent: false,
        blocking: false,
        expires_at: None,
    })
}
impl crate::workflow::Controller {
    pub fn attach_approvals(
        self: &Arc<Self>,
        service: &Arc<Service>,
    ) -> std::result::Result<(), String> {
        *self
            .approval_service
            .lock()
            .map_err(|_| "workflow runtime unavailable")? = Some(Arc::downgrade(service));
        service
            .register_domain(
                &self.workspace,
                "workflow",
                Arc::new(Adapter {
                    controller: Arc::downgrade(self),
                }),
            )
            .map_err(|e| e.to_string())?;
        for run in self.list()? {
            for request in self.requests(&run.id)? {
                if request.status == "pending" {
                    publish(service, &self.workspace, &request).map_err(|e| e.to_string())?;
                }
            }
        }
        service
            .apply_available(&self.workspace)
            .map_err(|e| e.to_string())
    }
    pub fn approval_request(
        &self,
        id: &str,
    ) -> std::result::Result<crate::workflow::ApprovalRequest, String> {
        let request = self
            .store
            .request(id)?
            .ok_or("workflow request not found")?;
        self.get(&request.run_id)?;
        Ok(request)
    }
}
