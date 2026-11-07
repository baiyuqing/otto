//! Durable completion input keys shared with recovery.

use super::*;

impl Runner {
    pub(super) fn notify_finished(&self, task_id: &str, persistence_error: Option<&str>) {
        let Some(task) = self.config.tasks.get(task_id) else {
            return;
        };
        let id = if task.attempt_id.is_empty() {
            String::new()
        } else {
            format!("child-result:{}:{}", task.id, task.attempt_id)
        };
        let mut text = self.parent_text(&task, &completion_text(&task, usize::MAX));
        if let Some(error) = persistence_error {
            text.push_str(&format!("\nPersistence warning: {error}"));
        }
        if let Err(error) = self.config.tasks.notifications().enqueue(Notification {
            id,
            task_id: task_id.to_string(),
            kind: Some(NotificationKind::TaskFinished),
            text,
            usage: task.usage_present.then_some(task.usage),
            cached_input_tokens_present: task.cached_input_tokens_present,
            ..Default::default()
        }) {
            let mut diagnostics = self.config.tasks.recovery_diagnostics();
            diagnostics.push(format!(
                "task {task_id}: completion notification pending recovery: {error}"
            ));
            self.config.tasks.set_recovery_diagnostics(diagnostics);
        }
    }
}
