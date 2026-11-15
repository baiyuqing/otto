use super::super::backend::{Command, Reply, Request};
use super::super::interaction::{Action, Activation, Row, View};
use super::{Presenter, Transition};
use otto::memory::Candidate;

#[derive(Default)]
pub(crate) struct Reviews {
    rows: Vec<Candidate>,
    loaded: bool,
    deciding: Option<String>,
}

impl Presenter for Reviews {
    fn covers_approval(&self, request: &otto_core::approval::Request) -> bool {
        (!self.loaded && request.subject.source == "memory" && request.is_human_pending())
            || self.rows.iter().any(|candidate| {
                otto::approval::memory::is_human_pending(candidate, std::slice::from_ref(request))
            })
    }
    fn key(&self) -> String {
        "reviews".into()
    }
    fn view(&self) -> View {
        View {
            collection: true,
            title: "Memory review".into(),
            description: "No change until you submit a decision".into(),
            document: if self.loaded && self.rows.is_empty() {
                "No pending memory candidates.".to_string()
            } else {
                String::new()
            },
            loading: !self.loaded,
            rows: self
                .rows
                .iter()
                .map(|row| Row {
                    id: row.id.clone(),
                    cells: vec![format!(
                        "{} {}/{}",
                        row.action.as_str(),
                        row.proposed.kind,
                        row.proposed.key
                    )],
                    detail: format!(
                        "{}\nReason: {}{}",
                        row.proposed.text,
                        row.reason,
                        otto::memory::advice::advice_line(&row.proposed)
                            .map(|s| format!("\n{s}"))
                            .unwrap_or_default()
                    ),
                })
                .collect(),
            actions: vec![
                Action {
                    id: "accept".into(),
                    label: "Accept".into(),
                    enabled: !self.rows.is_empty(),
                    shortcut: Some('a'),
                    ..Default::default()
                },
                Action {
                    id: "reject".into(),
                    label: "Reject".into(),
                    enabled: !self.rows.is_empty(),
                    shortcut: Some('r'),
                    ..Default::default()
                },
            ],
            default_action: Some("accept".into()),
            ..Default::default()
        }
    }
    fn activate(&mut self, activation: Activation) -> Transition {
        if let Activation::Action {
            id, row: Some(row), ..
        } = activation
            && self.rows.iter().any(|r| r.id == row)
            && matches!(id.as_str(), "accept" | "reject")
        {
            self.deciding = Some(row.clone());
            return Transition::Request(Request::Command(Command::MemoryReview {
                id: row,
                accept: id == "accept",
            }));
        }
        Transition::Stay
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        match reply {
            Reply::Reviews(rows) => {
                self.rows = rows;
                self.loaded = true;
                Transition::Stay
            }
            Reply::Operation { .. } => {
                if let Some(id) = self.deciding.take() {
                    self.rows.retain(|row| row.id != id);
                }
                Transition::Request(Request::Reviews)
            }
            _ => Transition::Stay,
        }
    }
    fn periodic(&self) -> bool {
        true
    }
    fn refresh(&self) -> Option<Request> {
        Some(Request::Reviews)
    }
}
