use super::super::backend::{Command, Reply, Request};
use super::super::interaction::{Action, Activation, Row, View};
use super::{Presenter, Transition};
use crate::approval::{Request as ApprovalRequest, State as ApprovalState};

pub(crate) struct Approval {
    pub id: String,
    title: String,
    details: String,
    revision: Option<u64>,
    pending: bool,
    outcome: String,
    review: bool,
}

impl Approval {
    pub fn shared(request: &ApprovalRequest) -> Self {
        Self {
            id: request.id.clone(),
            title: request.title.clone(),
            details: format!(
                "{}\n\nReason: {}\n\nAuthorization: {}",
                request.subject.details,
                request.reason,
                if request.kind == crate::approval::Kind::Confirmation {
                    "proposal confirmation"
                } else if request.persistent {
                    "persistent"
                } else {
                    "once"
                }
            ),
            revision: Some(request.revision),
            pending: request.state == ApprovalState::Pending,
            outcome: format!(
                "{} · {}{}",
                match request.state {
                    crate::approval::State::Pending => "Waiting for a decision",
                    crate::approval::State::Approved => "Approved",
                    crate::approval::State::Rejected => "Rejected",
                    crate::approval::State::Withdrawn => "Withdrawn",
                    crate::approval::State::Expired => "Expired",
                    crate::approval::State::Superseded => "Superseded",
                },
                match request.application {
                    crate::approval::ApplicationState::NotRequired => "No further application",
                    crate::approval::ApplicationState::NotStarted => "Waiting to apply",
                    crate::approval::ApplicationState::Applying => "Applying",
                    crate::approval::ApplicationState::Applied => "Applied",
                    crate::approval::ApplicationState::Failed => "Application failed",
                    crate::approval::ApplicationState::Unknown => "Outcome unknown",
                },
                if request.application_error.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", request.application_error)
                }
            ),
            review: matches!(request.subject.source.as_str(), "memory" | "reflection"),
        }
    }

    pub fn legacy(id: String, command: String, reason: String, read_path: String) -> Self {
        Self {
            id,
            title: "Permission approval".into(),
            details: format!(
                "Command: {command}\n\nReason: {reason}{}",
                if read_path.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\nPermanently allow reading: {read_path}\nSaved to read_paths; commands remain sandboxed."
                    )
                }
            ),
            revision: None,
            pending: true,
            outcome: String::new(),
            review: false,
        }
    }
}

impl Presenter for Approval {
    fn key(&self) -> String {
        format!("approval:{}", self.id)
    }
    fn view(&self) -> View {
        View {
            modal: true,
            title: self.title.clone(),
            description: self.outcome.clone(),
            document: self.details.clone(),
            actions: vec![
                Action {
                    id: "allow".into(),
                    label: if self.review { "Accept" } else { "Approve" }.into(),
                    enabled: self.pending,
                    shortcut: Some(if self.review { 'a' } else { 'y' }),
                    ..Default::default()
                },
                Action {
                    id: "deny".into(),
                    label: if self.review { "Reject" } else { "Deny" }.into(),
                    enabled: self.pending,
                    shortcut: Some(if self.review { 'r' } else { 'n' }),
                    ..Default::default()
                },
            ],
            default_action: Some("allow".into()),
            ..Default::default()
        }
    }
    fn activate(&mut self, activation: Activation) -> Transition {
        let Activation::Action { id, .. } = activation else {
            return Transition::Stay;
        };
        if !self.pending || !matches!(id.as_str(), "allow" | "deny") {
            return Transition::Stay;
        }
        if self.revision.is_none() {
            return Transition::Request(Request::Command(if id == "allow" {
                Command::Approve(self.id.clone())
            } else {
                Command::Deny(self.id.clone())
            }));
        }
        Transition::Request(Request::Command(Command::DecideApproval {
            id: self.id.clone(),
            allow: id == "allow",
            feedback: String::new(),
            revision: self.revision,
        }))
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        match reply {
            Reply::Operation { .. } => return Transition::Close,
            Reply::ApprovalSnapshot(snapshot) => {
                if let Some(request) = snapshot
                    .requests
                    .iter()
                    .find(|request| request.id == self.id)
                {
                    *self = Self::shared(request);
                }
            }
            _ => {}
        }
        Transition::Stay
    }
}

#[derive(Default)]
pub(crate) struct Approvals {
    requests: Vec<ApprovalRequest>,
    loaded: bool,
}

impl Presenter for Approvals {
    fn key(&self) -> String {
        "approvals".into()
    }
    fn view(&self) -> View {
        View {
            collection: true,
            title: "Pending approvals".into(),
            loading: !self.loaded,
            rows: self
                .requests
                .iter()
                .map(|r| Row {
                    id: r.id.clone(),
                    cells: vec![r.title.clone()],
                    detail: r.reason.clone(),
                })
                .collect(),
            ..Default::default()
        }
    }
    fn activate(&mut self, activation: Activation) -> Transition {
        if let Activation::Open(id) = activation
            && let Some(request) = self.requests.iter().find(|r| r.id == id)
        {
            return Transition::Push(Box::new(Approval::shared(request)));
        }
        Transition::Stay
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        if let Reply::ApprovalSnapshot(snapshot) = reply {
            self.requests = snapshot.requests;
            self.loaded = true;
        }
        Transition::Stay
    }
    fn periodic(&self) -> bool {
        true
    }
    fn refresh(&self) -> Option<Request> {
        Some(Request::ApprovalSnapshot)
    }
}
