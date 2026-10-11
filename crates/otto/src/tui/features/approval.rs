use super::super::backend::{Command, Reply, Request};
use super::super::interaction::{Action, Activation, Row, View};
use super::{Presenter, Transition};
use otto_core::approval::Request as ApprovalRequest;

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
                "{}\n\nReason: {}\n\nAuthorization: {}{}",
                request.subject.details,
                request.reason,
                if request.kind == otto_core::approval::Kind::Confirmation {
                    "proposal confirmation"
                } else if request.persistent {
                    "persistent"
                } else {
                    "once"
                },
                request
                    .review
                    .as_ref()
                    .map(|review| format!(
                        "\n\nReview: {:?}{}",
                        review.status,
                        review
                            .assessment
                            .as_ref()
                            .map(|assessment| format!("\n{}", assessment.reason))
                            .unwrap_or_default()
                    ))
                    .unwrap_or_default()
            ),
            revision: Some(request.revision),
            pending: request.is_human_pending(),
            outcome: format!(
                "{} · {}{}",
                match request.state {
                    otto_core::approval::State::Pending if !request.is_human_pending() =>
                        "Reviewing",
                    otto_core::approval::State::Pending => "Waiting for a decision",
                    otto_core::approval::State::Approved => "Approved",
                    otto_core::approval::State::Rejected => "Rejected",
                    otto_core::approval::State::Withdrawn => "Withdrawn",
                    otto_core::approval::State::Expired => "Expired",
                    otto_core::approval::State::Superseded => "Superseded",
                },
                match request.application {
                    otto_core::approval::ApplicationState::NotRequired => "No further application",
                    otto_core::approval::ApplicationState::NotStarted => "Waiting to apply",
                    otto_core::approval::ApplicationState::Applying => "Applying",
                    otto_core::approval::ApplicationState::Applied => "Applied",
                    otto_core::approval::ApplicationState::Failed => "Application failed",
                    otto_core::approval::ApplicationState::Unknown => "Outcome unknown",
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
            title: self.title.clone(),
            description: self.outcome.clone(),
            document: self.details.clone(),
            actions: if self.pending {
                vec![
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
                ]
            } else {
                Vec::new()
            },
            default_action: self.pending.then(|| "allow".into()),
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
    counts: crate::approval::ApprovalCounts,
    loaded: bool,
}

impl Presenter for Approvals {
    fn key(&self) -> String {
        "approvals".into()
    }
    fn view(&self) -> View {
        View {
            collection: true,
            title: "Approvals".into(),
            description: format!(
                "{} human pending · {} reviewing · {} application failed",
                self.counts.human_pending, self.counts.reviewing, self.counts.application_failed
            ),
            loading: !self.loaded,
            rows: self
                .requests
                .iter()
                .map(|r| Row {
                    id: r.id.clone(),
                    cells: vec![r.title.clone()],
                    detail: format!(
                        "{:?} · {}",
                        r.state,
                        r.review
                            .as_ref()
                            .and_then(|review| review.assessment.as_ref())
                            .map(|assessment| assessment.reason.as_str())
                            .unwrap_or(&r.reason)
                    ),
                })
                .collect(),
            ..Default::default()
        }
    }
    fn activate(&mut self, activation: Activation) -> Transition {
        if let Activation::Open(id) = activation
            && let Some(request) = self.requests.iter().find(|r| r.id == id)
        {
            if request.state == otto_core::approval::State::Pending && !request.is_human_pending() {
                return Transition::Push(Box::new(super::Document {
                    key: format!("review:{}", request.id),
                    title: "Review in progress".into(),
                    text: format!("{}\n{}", request.title, request.subject.details),
                }));
            }
            return Transition::Push(Box::new(Approval::shared(request)));
        }
        Transition::Stay
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        if let Reply::ApprovalActivity(activity) = reply {
            self.requests = activity.requests;
            self.requests.extend(activity.reviewing);
            self.requests.extend(activity.recent_decisions);
            self.counts = activity.counts;
            self.loaded = true;
        }
        Transition::Stay
    }
    fn periodic(&self) -> bool {
        true
    }
    fn refresh(&self) -> Option<Request> {
        Some(Request::ApprovalActivity)
    }
}
