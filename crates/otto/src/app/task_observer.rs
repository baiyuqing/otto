//! Read-only child observer assembly. Disk work belongs on a frontend blocking worker.
use std::collections::HashMap;
use std::sync::Mutex;

use super::Controller;
use super::tasks::{ObserverEntry, ObserverSnapshot, ObserverTask};
use crate::subagent::record::{ListQuery, TaskRow};
use crate::subagent::tasks::observer::{MAX_TASKS, bounded};

mod history;

#[derive(Default)]
pub(super) struct ObserverCache(Mutex<HashMap<(String, String), history::CachedHistory>>);

impl Controller {
    /// A bounded cumulative snapshot; never includes reasoning or child prompts.
    /// Call from a blocking worker: restarted children can require transcript reads.
    pub fn observer_snapshot(&self) -> Result<ObserverSnapshot, String> {
        let (parent, parent_path, workspace, registry) = {
            let state = self.lock();
            if !self.dynamic_content || state.closed || state.close_pending {
                return Err("session observer unavailable".into());
            }
            let current = state
                .current
                .as_ref()
                .ok_or("session observer unavailable")?;
            (
                current.session.header().id,
                current.path(),
                current.workspace.clone(),
                current.runner.tasks().cloned(),
            )
        };
        let (mut tasks, mut truncated) = registry
            .as_ref()
            .map(|tasks| tasks.observer_tasks())
            .unwrap_or_default();
        for task in &mut tasks {
            task.row.parent_session = parent.clone();
        }
        let live_ids: Vec<String> = tasks.iter().map(|task| task.row.task_id.clone()).collect();
        let page = self.builder.tasks_list_observer(&ListQuery {
            parent_session: Some(parent.clone()),
            workspace: Some(workspace.clone()),
            limit: Some((MAX_TASKS + 1) as u32),
            ..Default::default()
        })?;
        truncated |= !page.next_before.is_empty();
        for row in page.tasks {
            if row.parent_session != parent
                || row.parent_session_path != parent_path
                || row.workspace != workspace
            {
                continue;
            }
            if live_ids.contains(&row.task_id) {
                continue;
            }
            tasks.push(ObserverTask {
                row,
                ..Default::default()
            });
        }
        tasks.sort_by(|left, right| {
            right
                .row
                .created_at
                .cmp(&left.row.created_at)
                .then_with(|| right.row.task_id.cmp(&left.row.task_id))
        });
        if tasks.len() > MAX_TASKS {
            tasks.truncate(MAX_TASKS);
            truncated = true;
        }
        let mut cache = self
            .observer_cache
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.retain(|(session, task), _| {
            session == &parent && tasks.iter().any(|item| &item.row.task_id == task)
        });
        let session_root = self
            .builder
            .storage
            .session_root(&self.builder.session_root)
            .ok();
        for task in &mut tasks {
            if !live_ids.contains(&task.row.task_id) {
                let key = (parent.clone(), task.row.task_id.clone());
                let completed = !matches!(task.row.status.as_str(), "queued" | "running");
                let history = session_root
                    .as_deref()
                    .and_then(|root| history::read(root, &task.row, cache.get(&key)));
                cache.remove(&key);
                match history {
                    Some(history) => {
                        task.entries = history.entries.clone();
                        if completed {
                            cache.insert(key, history);
                        }
                    }
                    None => {
                        task.entries = vec![ObserverEntry {
                            text: "Child transcript unavailable.".into(),
                            ..Default::default()
                        }];
                        task.missing = true;
                    }
                }
            }
            sanitize_row(&mut task.row);
        }
        drop(cache);
        let state = self.lock();
        if state.closed
            || state.close_pending
            || state
                .current
                .as_ref()
                .is_none_or(|current| current.session.header().id != parent)
        {
            return Err("session changed while reading observer".into());
        }
        Ok(ObserverSnapshot { tasks, truncated })
    }
}

fn sanitize_row(row: &mut TaskRow) {
    // Observer metadata never carries prompts, context, full results or paths.
    row.prompt.clear();
    row.context.clear();
    row.result.clear();
    row.session_path.clear();
    row.parent_session_path.clear();
    row.workspace.clear();
    for text in [
        &mut row.parent_session,
        &mut row.task_id,
        &mut row.name,
        &mut row.agent,
        &mut row.description,
        &mut row.model,
        &mut row.last_tool,
        &mut row.error,
        &mut row.status,
        &mut row.created_at,
        &mut row.started_at,
        &mut row.finished_at,
    ] {
        *text = bounded(text);
    }
}

#[cfg(test)]
mod tests;
