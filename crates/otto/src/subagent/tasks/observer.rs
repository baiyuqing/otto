//! Bounded projections of already-redacted child events. Never observes provider input.
use super::observed::{ObserverEntry, ObserverEntryKind, ObserverTask};
use super::*;
use crate::subagent::record::TaskRow;
use otto_core::agent::Event;
use otto_core::model::{BlockType, Role};
use std::hash::{Hash, Hasher};

fn identity(parts: impl Hash) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

pub(crate) const MAX_TASKS: usize = 32;
const MAX_ENTRIES: usize = 32;
const MAX_FIELD_BYTES: usize = 4096;
const MAX_TAIL_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct Projection {
    entries: Vec<(String, ObserverEntry)>,
    assistant_open: bool,
    truncated: bool,
}

impl Projection {
    fn push(&mut self, identity: String, entry: ObserverEntry) {
        self.entries.push((identity, entry));
        self.bound();
    }

    fn bound(&mut self) {
        for (_, entry) in &mut self.entries {
            for text in [
                &mut entry.text,
                &mut entry.tool_name,
                &mut entry.tool_args,
                &mut entry.tool_output,
            ] {
                if text.len() > MAX_FIELD_BYTES {
                    let start = text.ceil_char_boundary(text.len() - MAX_FIELD_BYTES);
                    text.drain(..start);
                    self.truncated = true;
                }
            }
        }
        while self.entries.len() > MAX_ENTRIES || self.bytes() > MAX_TAIL_BYTES {
            self.entries.remove(0);
            self.truncated = true;
        }
    }

    fn bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(id, entry)| {
                id.len()
                    + entry.text.len()
                    + entry.tool_name.len()
                    + entry.tool_args.len()
                    + entry.tool_output.len()
            })
            .sum()
    }

    pub(crate) fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.assistant_open
            && let Some((_, entry)) = self.entries.last_mut()
        {
            // Never allocate an unbounded delta before trimming it.
            let start = text.ceil_char_boundary(text.len().saturating_sub(MAX_FIELD_BYTES));
            self.truncated |= start > 0;
            entry.text.push_str(&text[start..]);
            self.bound();
        } else {
            self.push(
                String::new(),
                ObserverEntry {
                    kind: ObserverEntryKind::Assistant,
                    text: bounded(text),
                    ..Default::default()
                },
            );
            self.truncated |= text.len() > MAX_FIELD_BYTES;
        }
        self.assistant_open = true;
    }

    fn tool(&mut self, identity: String, name: &str, args: &str) {
        self.assistant_open = false;
        self.truncated |= name.len() > MAX_FIELD_BYTES || args.len() > MAX_FIELD_BYTES;
        if let Some((_, entry)) = self.entries.iter_mut().find(|(id, _)| *id == identity) {
            entry.tool_name = bounded(name);
            entry.tool_args = bounded(args);
            self.bound();
            return;
        }
        self.push(
            identity,
            ObserverEntry {
                kind: ObserverEntryKind::Tool,
                tool_name: bounded(name),
                tool_args: bounded(args),
                ..Default::default()
            },
        );
    }

    fn tool_result(&mut self, identity: String, name: &str, output: &str, error: bool) {
        self.assistant_open = false;
        if !self.entries.iter().any(|(id, _)| *id == identity) {
            self.tool(identity.clone(), name, "");
        }
        self.truncated |= output.len() > MAX_FIELD_BYTES;
        if let Some((_, entry)) = self.entries.iter_mut().find(|(id, _)| *id == identity) {
            entry.tool_output = bounded(output);
            entry.tool_done = true;
            entry.tool_error = error;
        }
        self.bound();
    }

    pub(crate) fn system(&mut self, text: &str) {
        self.assistant_open = false;
        self.push(
            String::new(),
            ObserverEntry {
                text: bounded(text),
                ..Default::default()
            },
        );
    }

    pub(crate) fn event(&mut self, event: &Event) {
        match event {
            Event::TextDelta { text } => self.text(text),
            Event::ProviderUsage { .. } => self.assistant_open = false,
            Event::ProviderRetry {
                interrupted_output: true,
                ..
            } => {
                self.system("Provider retry: preceding partial output was interrupted.");
            }
            Event::ToolCallStarted {
                operation_id,
                attempt,
                tool_name,
                tool_call_id,
                arguments,
            } => {
                self.tool(
                    identity((operation_id, attempt, tool_call_id)),
                    tool_name,
                    arguments,
                );
            }
            Event::ToolCallFinished {
                operation_id,
                attempt,
                tool_name,
                tool_call_id,
                result,
                ..
            } => {
                self.tool_result(
                    identity((operation_id, attempt, tool_call_id)),
                    tool_name,
                    &result.content,
                    result.is_error,
                );
            }
            _ => {} // In particular: no reasoning, prompts, context, or notifications.
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<ObserverEntry> {
        let mut entries = Vec::with_capacity(self.entries.len() + 1);
        if self.truncated {
            entries.push(ObserverEntry {
                text: "Earlier observer output truncated; showing bounded recent output.".into(),
                ..Default::default()
            });
        }
        entries.extend(self.entries.iter().map(|(_, entry)| entry.clone()));
        entries
    }

    pub(crate) fn transcript(messages: &[Message]) -> Self {
        let mut projection = Self::default();
        for message in messages {
            projection.assistant_open = false;
            for block in &message.blocks {
                match (&message.role, &block.block_type) {
                    (Role::Assistant, BlockType::Text) => projection.text(&block.text),
                    (Role::Assistant, BlockType::ToolCall) => projection.tool(
                        identity((
                            &block.invocation_id,
                            block.invocation_attempt,
                            &block.tool_call_id,
                        )),
                        &block.tool_name,
                        block.arguments.as_ref().map_or("", |args| args.get()),
                    ),
                    (Role::Tool, BlockType::ToolResult) => projection.tool_result(
                        identity((
                            &block.invocation_id,
                            block.invocation_attempt,
                            &block.tool_call_id,
                        )),
                        &block.tool_name,
                        &block.text,
                        block.is_error,
                    ),
                    _ => {}
                }
            }
        }
        projection
    }
}

pub(crate) fn bounded(text: &str) -> String {
    text[text.ceil_char_boundary(text.len().saturating_sub(MAX_FIELD_BYTES))..].to_string()
}

impl Tasks {
    /// Only safe, post-redaction child events belong at this boundary.
    pub fn observe(&self, id: &str, event: &Event) {
        if let Some(projection) = self
            .lock()
            .entries
            .get_mut(id)
            .and_then(|entry| entry.observer.as_mut())
        {
            projection.event(event);
        }
    }

    pub(crate) fn observe_system(&self, id: &str, text: &str) {
        if let Some(projection) = self
            .lock()
            .entries
            .get_mut(id)
            .and_then(|entry| entry.observer.as_mut())
        {
            projection.system(text);
        }
    }

    pub(crate) fn observe_external_text(&self, id: &str, text: &str) {
        if let Some(projection) = self
            .lock()
            .entries
            .get_mut(id)
            .and_then(|entry| entry.observer.as_mut())
        {
            projection.text(text);
        }
    }

    pub(crate) fn observe_external_tool(
        &self,
        task: &str,
        call: &str,
        name: &str,
        done: bool,
        error: bool,
    ) {
        if let Some(projection) = self
            .lock()
            .entries
            .get_mut(task)
            .and_then(|entry| entry.observer.as_mut())
        {
            if done {
                projection.tool_result(
                    identity(call),
                    name,
                    "Adapter did not expose tool output.",
                    error,
                );
            } else {
                projection.tool(identity(call), name, "");
            }
        }
    }

    pub(crate) fn observer_tasks(&self) -> (Vec<ObserverTask>, bool) {
        let state = self.lock();
        let truncated = state.order.len() > MAX_TASKS;
        let context = self.recorder.as_ref().map(|(_, context)| context);
        let tasks = state
            .order
            .iter()
            .rev()
            .take(MAX_TASKS)
            .filter_map(|id| {
                let entry = state.entries.get(id)?;
                let task = &entry.task;
                let row = TaskRow {
                    parent_session: context
                        .map_or_else(String::new, |context| context.parent_session.clone()),
                    task_id: task.id.clone(),
                    name: bounded(&task.name),
                    agent: bounded(&task.agent),
                    description: bounded(&task.description),
                    model: bounded(&task.model),
                    model_source: task.model_source,
                    status: task.status.as_str().to_string(),
                    created_at: task
                        .created_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_default(),
                    started_at: task
                        .started_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_default(),
                    finished_at: task
                        .finished_at
                        .map(|time| time.to_rfc3339())
                        .unwrap_or_default(),
                    steps: task.steps,
                    tool_calls: task.tool_calls,
                    last_tool: bounded(&task.last_tool),
                    input_tokens: task.usage.input_tokens,
                    output_tokens: task.usage.output_tokens,
                    cached_tokens: task.usage.cached_input_tokens,
                    error: bounded(&task.error),
                    ..Default::default()
                };
                Some(ObserverTask {
                    row,
                    entries: entry
                        .observer
                        .as_ref()
                        .map(Projection::snapshot)
                        .unwrap_or_default(),
                    missing: entry.observer.is_none(),
                })
            })
            .collect();
        (tasks, truncated)
    }
}

#[cfg(test)]
mod tests;
