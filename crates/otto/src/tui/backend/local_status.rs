//! Local status projections run on the backend's blocking worker, never per draw.

use super::*;
use crate::subagent::tasks::Task;

#[derive(Debug)]
pub(crate) struct LocalStatus {
    pub info: Info,
    pub tasks: Vec<Task>,
    pub skill_names: Vec<String>,
    pub input_statuses: Vec<(String, String)>,
}

pub(crate) fn local_status(controller: &Controller) -> LocalStatus {
    let info = controller.info();
    let mut input_statuses = Vec::new();
    if let Ok(records) = controller.input_submissions() {
        let guidance = controller.steering_messages().unwrap_or_default();
        for record in records {
            let terminal_guidance = guidance
                .iter()
                .find(|message| message.input_id == record.id)
                .and_then(|message| match message.status {
                    otto_core::agent::steering::Status::Withdrawn => Some("withdrawn"),
                    otto_core::agent::steering::Status::Discarded => {
                        Some("failed: guidance discarded before delivery")
                    }
                    _ => None,
                });
            let status = if controller.has_input(&record.id) {
                "delivered"
            } else {
                terminal_guidance.unwrap_or(&record.state)
            };
            let marker = format!(
                "[{}] {:?} / {:?} / {}",
                record.id, record.provenance.source, record.provenance.delivery, status
            );
            input_statuses.push((record.id, marker));
        }
    }
    let tasks = controller
        .subagent_tasks()
        .map(|tasks| tasks.list())
        .unwrap_or_default();
    let mut skill_names: Vec<_> = controller
        .skills()
        .skills()
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    skill_names.sort();
    LocalStatus {
        info,
        tasks,
        skill_names,
        input_statuses,
    }
}
