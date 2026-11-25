//! Local status projections run on the backend's blocking worker, never per draw.

use super::*;
use otto::subagent::tasks::Task;

#[derive(Debug)]
pub(crate) struct LocalStatus {
    pub info: Info,
    pub tasks: Vec<Task>,
    pub skill_names: Vec<String>,
    pub unresolved_input_ids: Vec<String>,
}

pub(crate) fn local_status(controller: &Controller) -> LocalStatus {
    use otto_core::agent::steering::Status;

    let info = controller.info();
    let mut unresolved_input_ids = Vec::new();
    if let Ok(records) = controller.input_submissions() {
        let guidance = controller.steering_messages().unwrap_or_default();
        for record in records {
            if matches!(record.state.as_str(), "pending" | "accepted" | "uncertain")
                && !controller.has_input(&record.id)
                && !guidance.iter().any(|message| {
                    message.input_id == record.id
                        && matches!(message.status, Status::Withdrawn | Status::Discarded)
                })
            {
                unresolved_input_ids.push(record.id);
            }
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
        unresolved_input_ids,
    }
}
