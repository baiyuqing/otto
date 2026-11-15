//! Registry updates for a running task: start, steps, tool calls.

use super::*;

impl Tasks {
    /// Moves a queued task to running. Invalid or repeated transitions,
    /// including unknown ids, are no-ops.
    pub fn mark_running(&self, id: &str, started_at: DateTime<Utc>) {
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Queued => {
                    entry.task.status = TaskStatus::Running;
                    entry.task.started_at = Some(started_at);
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.signal();
            self.record(id);
        }
    }

    /// Records the file the task's transcript is written to. Unknown ids are
    /// ignored.
    pub fn set_session_path(&self, id: &str, path: &str) {
        if let Some(entry) = self.lock().entries.get_mut(id) {
            entry.task.session_path = path.to_string();
        }
    }

    /// Records one completed provider round trip. It only applies while the
    /// task is running; invalid or unknown updates are no-ops.
    ///
    /// `cached_input_tokens_present` reports whether this round trip carried
    /// the cached-input-token breakdown. The task keeps the breakdown only
    /// when every report that carried usage carried it too.
    pub fn record_provider_step(
        &self,
        id: &str,
        usage: Usage,
        text: &str,
        usage_present: bool,
        cached_input_tokens_present: bool,
    ) {
        if usage.validate().is_err() {
            return;
        }
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Running => {
                    entry.task.steps += 1;
                    if usage_present {
                        let first_report = !entry.task.usage_present;
                        entry.task.usage_present = true;
                        entry.task.cached_input_tokens_present = if first_report {
                            cached_input_tokens_present
                        } else {
                            entry.task.cached_input_tokens_present && cached_input_tokens_present
                        };
                        entry.task.usage = if first_report {
                            usage
                        } else {
                            entry.task.usage.saturating_add(usage)
                        };
                    }
                    entry.task.last_text = text.to_string();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.signal();
            self.record(id);
        }
    }

    /// Records the latest tool call while the task is running. Invalid or
    /// unknown updates are no-ops.
    pub fn record_tool_call(&self, id: &str, last_tool: &str) {
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Running => {
                    entry.in_tool = true;
                    entry.task.tool_calls += 1;
                    entry.task.last_tool = last_tool.to_string();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.signal();
            self.record(id);
        }
    }

    /// Finishes the current tool phase without changing historical tool data.
    pub(crate) fn record_tool_finished(&self, id: &str) {
        let changed = {
            let mut state = self.lock();
            if let Some(entry) = state.entries.get_mut(id)
                && entry.task.status == TaskStatus::Running
                && entry.in_tool
            {
                entry.in_tool = false;
                true
            } else {
                false
            }
        };
        if changed {
            self.signal();
        }
    }

    /// Live process-local phase. Provider/runtime includes model requests,
    /// retries and orchestration; it is not a claim that a provider is stalled.
    pub(crate) fn activity(&self, reference: &str) -> Option<&'static str> {
        let state = self.lock();
        let entry = Self::entry(&state, reference)?;
        Some(match entry.task.status {
            TaskStatus::Running if entry.in_tool => "tool",
            TaskStatus::Running => "provider_or_runtime",
            _ => entry.task.status.as_str(),
        })
    }
}
