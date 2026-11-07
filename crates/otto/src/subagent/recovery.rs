//! Activation rebuilds a derived task view, never execution.

use std::io::Write;
use std::path::Path;

use otto_core::agent::inbox::{Notification, NotificationKind};
use otto_core::mailbox::Mailbox;

use super::interrupted;
use super::tasks::{Task, TaskStatus, Tasks};

pub(crate) fn snapshot(child: &interrupted::ChildRecord) -> Task {
    let status = match child.final_status.as_deref() {
        Some("succeeded") => TaskStatus::Succeeded,
        Some("failed") => TaskStatus::Failed,
        Some("canceled") => TaskStatus::Canceled,
        _ => TaskStatus::Interrupted,
    };
    Task {
        id: child.task_id.clone(),
        attempt_id: child.attempt_id.clone(),
        name: child.name.clone(),
        agent: child.agent.clone(),
        description: child.description.clone(),
        prompt: child.prompt.clone(),
        context: child.context.clone(),
        model: child.model.clone(),
        model_source: child.model_source,
        status,
        session_path: child.path.to_string_lossy().into_owned(),
        result: child.last_assistant_text.clone(),
        error: if status == TaskStatus::Interrupted {
            "Recovered after owner loss; no execution is running. Inspect uncertain tool effects before explicit resume.".into()
        } else {
            String::new()
        },
        ..Task::default()
    }
}

pub(crate) fn activate(
    parent: &Path,
    tasks: &Tasks,
    inbox: &dyn Mailbox,
    warnings: &mut dyn Write,
) {
    let inventory = interrupted::inventory(parent);
    let mut diagnostics = inventory.diagnostics;
    let mut known = std::collections::HashSet::new();
    for mut child in inventory.records {
        known.insert(child.task_id.clone());
        if child.final_status.is_none() {
            match interrupted::mark_interrupted(&child.path) {
                Ok(()) => child.final_status = Some(interrupted::INTERRUPTED_STATUS.into()),
                Err(error) => diagnostics.push(format!(
                    "recovery: {} repair failed; resume unavailable: {error}",
                    child.task_id
                )),
            }
        }
        if child.final_status.as_deref().is_some_and(|status| {
            !matches!(status, "succeeded" | "failed" | "canceled" | "interrupted")
        }) {
            diagnostics.push(format!(
                "recovery: {} has unknown terminal status; resume unavailable",
                child.task_id
            ));
            continue;
        }
        let task = snapshot(&child);
        if let Err(error) = tasks.import_recovered(task) {
            diagnostics.push(format!("recovery: {error}"));
            continue;
        }
        // Key from the latest durable lifecycle entry, not process time. Both
        // queued and delivered inbox input ids prevent repeated-open duplicates.
        if child.final_status.is_some()
            && let Ok((entries, _)) = crate::session::Store::read_entries(&child.path)
            && let Some(entry) = entries.iter().rev().find(|entry| {
                entry
                    .custom
                    .as_ref()
                    .is_some_and(|custom| custom.custom_type == "otto.task_result")
            })
        {
            let text = format!(
                "Recovered task {}: {}. No execution was restarted. Inspect agent_status and the saved transcript before resuming.\n{}",
                child.task_id,
                child.final_status.as_deref().unwrap_or("unavailable"),
                super::runner::completion_text(&snapshot(&child), 4096)
            );
            let attempt = if child.attempt_id.is_empty() {
                &entry.id
            } else {
                &child.attempt_id
            };
            let input_id = format!("child-result:{}:{}", child.task_id, attempt);
            if tasks.has_delivered_input(&input_id) {
                continue;
            }
            if let Err(error) = inbox.enqueue(Notification {
                id: input_id,
                task_id: child.task_id.clone(),
                kind: Some(NotificationKind::Message),
                text,
                ..Notification::default()
            }) {
                diagnostics.push(format!(
                    "recovery: {} notification pending: {error}",
                    child.task_id
                ));
            }
        }
    }
    // Admissions describe the pre-spec window. Never reconstruct executable
    // definitions from these minimal receipts or hide a missing transcript.
    match crate::session::Store::read_entries(parent) {
        Ok((entries, _)) => {
            for entry in entries {
                let Some(custom) = entry.custom else {
                    continue;
                };
                if custom.custom_type != "otto.task_admitted" {
                    continue;
                }
                let Some(data) = custom.data else {
                    continue;
                };
                let Ok(data) = serde_json::from_str::<serde_json::Value>(data.get()) else {
                    diagnostics.push("recovery: malformed admission receipt".into());
                    continue;
                };
                let Some(id) = data.get("id").and_then(|id| id.as_str()) else {
                    continue;
                };
                if known.contains(id) {
                    continue;
                }
                known.insert(id.to_owned());
                let error = "durable admission has no usable child spec; execution unavailable; inspect original request and effects before constructing a replacement";
                diagnostics.push(format!("recovery: {id}: {error}"));
                let task = Task {
                    id: id.into(),
                    status: TaskStatus::Interrupted,
                    error: error.into(),
                    description: data
                        .get("description")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .into(),
                    ..Task::default()
                };
                if let Err(error) = tasks.import_recovered(task) {
                    diagnostics.push(format!("recovery: {error}"));
                }
            }
        }
        Err(error) => diagnostics.push(format!("recovery: parent admissions unavailable: {error}")),
    }
    if !inventory.complete || !diagnostics.is_empty() {
        diagnostics.insert(
            0,
            format!(
                "recovery coverage: {}; {} recovered task snapshots; no execution restarted",
                if inventory.complete {
                    "complete with diagnostics"
                } else {
                    "incomplete"
                },
                tasks.list().len()
            ),
        );
    }
    for diagnostic in &diagnostics {
        let _ = writeln!(warnings, "warning: {diagnostic}");
    }
    tasks.set_recovery_diagnostics(diagnostics);
}

#[cfg(test)]
mod tests;
