//! Application of wire events to the transcript and status state.

use super::*;

impl App {
    pub fn apply_event(&mut self, event: &WireEvent) -> bool {
        if let Some(reporter) = &self.program_status {
            reporter.observe_wire_stdout(event);
        }
        let phase = if event.event_type == "tool_call_started" {
            Some(format!("running {}", event.tool_name))
        } else {
            transcript::phase(event)
        };
        if let Some(phase) = phase
            && phase != self.phase.0
        {
            self.phase = (phase, Instant::now());
            self.retry_event = (event.event_type == "provider_retry").then(|| event.clone());
        }
        match event.event_type.as_str() {
            "provider_retry"
                if event
                    .retry
                    .as_ref()
                    .is_some_and(|retry| retry.interrupted_output) =>
            {
                self.push_system(transcript::INTERRUPTED_OUTPUT_NOTICE);
            }
            "plan_updated" => {
                if let Some(steps) = &event.steps {
                    self.info.plan = steps.clone();
                }
            }
            "reasoning_delta" => append_stream(
                &mut self.entries,
                "streaming-reasoning",
                EntryKind::Reasoning,
                &event.text,
            ),
            "text_delta" => append_stream(
                &mut self.entries,
                "streaming",
                EntryKind::Assistant,
                &event.text,
            ),
            "provider_usage" if event.usage_present == Some(true) => {
                let reported = event.cached_input_tokens_present == Some(true);
                self.cache_breakdown_complete =
                    Some(self.cache_breakdown_complete.unwrap_or(true) && reported);
                if let Some(usage) = event.usage {
                    self.usage.input_tokens = self
                        .usage
                        .input_tokens
                        .saturating_add(usage.input_tokens.max(0));
                    self.usage.output_tokens = self
                        .usage
                        .output_tokens
                        .saturating_add(usage.output_tokens.max(0));
                    self.usage.cached_input_tokens = self
                        .usage
                        .cached_input_tokens
                        .saturating_add(usage.cached_input_tokens.max(0));
                }
            }
            "tool_call_started" => super::super::entries::reduce_tool_event_at(
                &mut self.entries,
                event,
                (!self.attached).then(Instant::now),
            ),
            "tool_call_finished" => {
                let result = event.result.clone().unwrap_or_default();
                let approval = if !self.attached {
                    let parsed = ToolResult {
                        content: result.content.clone(),
                        is_error: result.is_error,
                        ..ToolResult::default()
                    };
                    crate::tool::bash::parse_approval_request(&event.tool_name, &parsed)
                } else {
                    None
                };
                super::super::entries::reduce_tool_event_at(
                    &mut self.entries,
                    event,
                    (!self.attached).then(Instant::now),
                );
                if let Some(request) = approval {
                    self.show_approval(features::Approval::legacy(
                        request.id,
                        request.command,
                        request.justification,
                        request.read_path,
                    ));
                }
            }
            "compaction_completed" => {
                if let Some(c) = &event.compaction {
                    self.push_system(compaction_line(c))
                }
            }
            "compaction_warning" | "memory_warning" => self.push_system(event.error.clone()),
            "agent_error" => {
                self.push_system(event.error.clone());
                return true;
            }
            USER_MESSAGE => {
                if let Some(index) = self
                    .pending_steers
                    .iter()
                    .position(|steer| steer.text == event.text)
                {
                    let delivered = self.pending_steers.remove(index);
                    self.delivered_steers.insert(delivered.id);
                }
                // A snapshot taken before this notification must not resurrect
                // its queued projection, even when we never observed admission.
                self.refresh_steering();
                if self.scroll.is_some() {
                    self.new_messages = true;
                }
                // The run appended this text as the user's message, which is its
                // own word that queued guidance reached the model. The steer is
                // resolved here so a turn end cannot offer it again.
                if let Some(steer) = self
                    .queued_steer
                    .as_ref()
                    .filter(|steer| steer.text == event.text)
                    .cloned()
                {
                    self.queued_steer = None;
                    self.recalling_steer = None;
                    self.resolve_steering(&steer);
                }
                if self.attached {
                    if self.awaiting_user_echo.as_deref() == Some(event.text.as_str()) {
                        self.awaiting_user_echo = None;
                    }
                    self.entries.push(Entry {
                        id: format!("user-{}", self.entries.len()),
                        kind: Some(EntryKind::User),
                        raw: event.text.clone(),
                        ..Entry::default()
                    })
                } else {
                    // A queued next input belongs to a later submission. The
                    // active turn can emit unrelated user messages meanwhile.
                    let expected_echo =
                        self.awaiting_user_echo.as_deref() == Some(event.text.as_str());
                    let duplicate = self.optimistic_user_echo.as_deref()
                        == Some(event.text.as_str())
                        && self.entries.last().is_some_and(|entry| {
                            entry.kind == Some(EntryKind::User) && entry.raw == event.text
                        });
                    if duplicate {
                        self.optimistic_user_echo = None;
                    }
                    if !expected_echo {
                        self.composer.remember(&event.text);
                    }
                    if expected_echo {
                        self.awaiting_user_echo = None;
                    }
                    if !duplicate {
                        self.entries.push(Entry {
                            id: format!("user-{}", self.entries.len()),
                            kind: Some(EntryKind::User),
                            raw: event.text.clone(),
                            ..Entry::default()
                        });
                    }
                }
            }
            "notification" => {
                // A finished sub-agent task reports its tokens here. The
                // session snapshot already counts them, so only the cache
                // breakdown's presence is taken from this report: without it
                // the footer must not present a share as measured.
                if event.usage_present == Some(true) {
                    let reported = event.cached_input_tokens_present == Some(true);
                    self.cache_breakdown_complete =
                        Some(self.cache_breakdown_complete.unwrap_or(true) && reported);
                }
                self.push_system(format!("[task {}] {}", event.task_id, event.text))
            }
            APPROVAL_REQUESTED => {
                if let Some(req) = &event.approval {
                    if req.is_human_pending() {
                        self.show_approval(features::Approval::shared(req));
                    }
                } else {
                    self.show_approval(features::Approval::legacy(
                        event.approval_id.clone(),
                        event.command.clone(),
                        event.justification.clone(),
                        event.read_path.clone(),
                    ));
                }
            }
            APPROVAL_DECIDED => {
                let key = format!("approval:{}", event.approval_id);
                let ids: Vec<_> = self
                    .surfaces
                    .iter()
                    .filter(|s| s.model.key() == key)
                    .map(|s| s.id)
                    .collect();
                for id in ids {
                    self.surfaces.remove(id);
                }
                self.push_system(format!(
                    "Approval {} decided elsewhere: {}",
                    event.approval_id, event.decision
                ));
            }
            "approval_changed" => {
                if let Some(request) = &event.approval {
                    if request
                        .decision
                        .as_ref()
                        .is_some_and(|d| d.transport == "auto-mode")
                    {
                        self.push_system(format!(
                            "[approval] {} ({}): automatically approved · application {:?}\n{}{}",
                            request.title,
                            request.id,
                            request.application,
                            request.subject.details,
                            if request.application_error.is_empty() {
                                String::new()
                            } else {
                                format!("\n{}", request.application_error)
                            }
                        ));
                    }
                    if let Some(decision) = &request.decision {
                        self.push_system(format!(
                            "[approval] {} ({}) {:?} · {}\n{}",
                            request.title,
                            request.id,
                            request.state,
                            decision.transport,
                            decision.feedback
                        ));
                    }
                    if request.state == otto_core::approval::State::Pending
                        && !request.is_human_pending()
                    {
                        self.push_system(format!(
                            "[approval] reviewing: {} ({})\n{}\nReason: {}",
                            request.title, request.id, request.subject.details, request.reason
                        ));
                    }
                    if request.is_human_pending()
                        && !self.hidden_approvals.contains(&request.id)
                        && !self
                            .surfaces
                            .iter()
                            .any(|s| s.model.key() == format!("approval:{}", request.id))
                    {
                        self.show_approval(features::Approval::shared(request));
                    } else if !request.is_human_pending() {
                        let key = format!("approval:{}", request.id);
                        let ids: Vec<_> = self
                            .surfaces
                            .iter()
                            .filter(|s| s.model.key() == key)
                            .map(|s| s.id)
                            .collect();
                        for id in ids {
                            self.surfaces.remove(id);
                        }
                    }
                }
            }
            _ => {}
        }
        false
    }
}
