//! Spawning a child agent, running it and recording its final state.

use super::*;

impl Runner {
    /// Creates the task record, builds the child agent, and spawns it.
    /// `resume_id` is `Some` only from [`Runner::resume`]: the task is
    /// registered under that id instead of a freshly counted one, and no
    /// `otto.task_spec` entry is written, since the transcript already has
    /// one from the task's original run.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn spawn(
        self: &Arc<Self>,
        request: StartRequest,
        definition: Option<Definition>,
        description: String,
        model: String,
        model_source: super::super::tasks::ModelSource,
        context: String,
        now: DateTime<Utc>,
        snapshot: Vec<Message>,
        transcript: Option<(Transcript, String)>,
        resume_id: Option<String>,
        deadline: Deadline,
    ) -> Result<Task, StartError> {
        let fresh_session_id = match transcript
            .as_ref()
            .and_then(|(session, _)| session.session_id())
        {
            Some(session_id) => session_id,
            None => crate::runtime::builder::random_id()
                .map_err(|error| StartError::SessionIdentity(error.to_string()))?,
        };
        let cancel = CancellationToken::new();
        // The task id names the child's file, so the transcript is built
        // after the task is registered; history reads it once it is set.
        let slot: Arc<std::sync::OnceLock<Transcript>> = Arc::default();
        let history_source = Arc::clone(&slot);
        let task_template = Task {
            name: request.name.trim().to_string(),
            agent: definition
                .as_ref()
                .map_or_else(|| request.agent.trim().to_owned(), |d| d.name.clone()),
            description,
            prompt: request.prompt.clone(),
            context,
            model: model.clone(),
            model_source,
            created_at: Some(now),
            ..Task::default()
        };
        let history = Some(Arc::new(move || {
            history_source
                .get()
                .map(|transcript| transcript.messages())
                .unwrap_or_default()
        }) as Arc<dyn Fn() -> Vec<Message> + Send + Sync>);
        let task = match resume_id.clone() {
            Some(id) => {
                self.config
                    .tasks
                    .add_with_id(id, task_template, Some(cancel.clone()), history)?
            }
            None => self
                .config
                .tasks
                .add(task_template, Some(cancel.clone()), history)?,
        };
        let mut start_guard = StartGuard {
            runner: Arc::clone(self),
            task_id: task.id.clone(),
            now,
            cancel: cancel.clone(),
            armed: true,
        };
        let transcript = match (transcript, &self.config.child_session) {
            (Some((transcript, path)), _) => {
                if !path.is_empty() {
                    self.config.tasks.set_session_path(&task.id, &path);
                }
                transcript
            }
            (None, None) => Arc::new(MemorySession::with_session_id(fresh_session_id.clone())),
            (None, Some(build)) => match build(&task.id, &fresh_session_id) {
                Ok(Some((transcript, path))) => {
                    self.config.tasks.set_session_path(&task.id, &path);
                    transcript
                }
                Ok(None) => Arc::new(MemorySession::with_session_id(fresh_session_id.clone())),
                Err(error) => {
                    // No transcript was built, so there is nothing to append
                    // otto.task_result to.
                    self.finish_without_transcript(
                        &task.id,
                        TaskStatus::Failed,
                        now,
                        &[],
                        Some(&error),
                    );
                    start_guard.disarm();
                    return Ok(self.config.tasks.get(&task.id).unwrap_or(task));
                }
            },
        };
        let session_id = transcript.session_id().unwrap_or(fresh_session_id);
        let _ = slot.set(Arc::clone(&transcript));
        if let Some(build) = &self.config.child_mailbox
            && let Some(path) = self
                .config
                .tasks
                .get(&task.id)
                .map(|task| task.session_path)
                .filter(|path| !path.is_empty())
        {
            self.config
                .tasks
                .replace_child_inbox(&task.id, build(&path));
        }

        // Durable custom entries are awaited before `start` returns, so even
        // a task that remains queued has its complete specification on disk.
        if resume_id.is_none() {
            let task_spec = TaskSpecData {
                id: task.id.clone(),
                agent: task.agent.clone(),
                name: task.name.clone(),
                description: task.description.clone(),
                model: task.model.clone(),
                model_source: task.model_source,
                context: task.context.clone(),
                prompt: task.prompt.clone(),
                definition: definition.as_ref().map(|definition| TaskSpecDefinition {
                    name: definition.name.clone(),
                    body: definition.body.clone(),
                    tools: definition.tools.clone(),
                    write_policy: definition.write_policy.as_str().to_string(),
                    write_paths: definition.write_paths.clone(),
                }),
                parent_tool_call_id: request.parent_tool_call_id.clone(),
            };
            if let Err(error) =
                append_task_custom(&transcript, TASK_SPEC_CUSTOM_TYPE, &task_spec).await
            {
                cancel.cancel();
                let messages = transcript.messages();
                self.finish(
                    &task.id,
                    TaskStatus::Failed,
                    now,
                    &messages,
                    Some(&error.to_string()),
                    Some(&transcript),
                )
                .await;
                start_guard.disarm();
                return Ok(self.config.tasks.get(&task.id).unwrap_or(task));
            }
        }

        if let Some(executable) = definition
            .is_none()
            .then(|| super::external::adapter(request.agent.trim()))
            .flatten()
        {
            let runner = Arc::clone(self);
            let task_id = task.id.clone();
            let prompt = request.prompt;
            tokio::spawn(async move {
                runner
                    .run_external(task_id, prompt, executable, cancel, transcript, deadline)
                    .await;
            });
            start_guard.disarm();
            return Ok(task);
        }

        let tools = ChildTools {
            session_id,
            registry: Arc::clone(&self.child_registry),
            allowed: definition
                .as_ref()
                .and_then(|definition| self.allowed_tools(definition)),
            write_policy: definition
                .as_ref()
                .map_or(WritePolicy::SingleWriter, |definition| {
                    definition.write_policy
                }),
            write_paths: definition
                .as_ref()
                .map_or_else(Vec::new, |definition| definition.write_paths.clone()),
            parent_inbox: Arc::clone(self.config.tasks.notifications()),
            task_id: task.id.clone(),
            max_output_bytes: self.config.max_output_bytes,
            reports_left: self
                .config
                .tasks
                .report_allowance(&task.id)
                .unwrap_or_else(|| Arc::new(AtomicUsize::new(REPORT_BUDGET))),
        };

        let scratch_session_id = self.scratch_session_id.get().map_or("", String::as_str);
        let scratch = self
            .scratch_root
            .get()
            .filter(|_| tools.permits("bash") && self.child_registry.lookup("bash").is_some())
            .map(|root| super::super::scratch::Scratch::new(root, scratch_session_id))
            .transpose()
            .map_err(|_| StartError::ScratchUnavailable)?;
        let report_path =
            self.reports
                .prepare(self.scratch_root.get(), scratch_session_id, &task.id);
        let report_policy = report_path.map_or_else(String::new, |path| {
            format!(
                "\n<research_report>For research too detailed for the handoff, use \
                write, when available and permitted, to save the complete report in \
                {path}/report.md with Markdown section headings. Return a concise \
                summary, uncertainties, the report path and a section index with line \
                numbers within 4 KiB. This directory persists after task exit; final.md \
                and index.md are reserved for runtime overflow capture. Do not place \
                reports in the temporary research directory. Existing write permissions \
                and assigned file scope still apply. If you cannot write a report, \
                return your findings as final text; the runtime saves an oversized \
                final answer automatically.</research_report>"
            )
        });
        let scratch_policy = scratch.as_ref().map_or_else(String::new, |scratch| {
            format!(
                "\n<research_workspace>Use {} for temporary source downloads and \
                analysis scripts. Download each pinned source once, then search/analyze \
                locally and return only conclusions with original URL or file/line \
                evidence. Keep durable deliverables outside this directory. Otto \
                deletes this directory when your task exits, including failure or \
                cancellation. Do not return its temporary paths as \
                evidence.</research_workspace>",
                scratch.path()
            )
        });

        let role_body = definition
            .as_ref()
            .map(|d| d.body.as_str())
            .filter(|body| !body.is_empty())
            .unwrap_or(GENERIC_SUBAGENT_INSTRUCTION);
        let redactor = self.redactor();
        let system_prompt = redactor.redact_string(&format!(
            "{}\n<subagent_role>\n{}\n</subagent_role>{scratch_policy}{report_policy}\n<child_report_policy>Keep \
                research process, source text and logs in your own context. Return only \
                the verified conclusion, essential evidence references, uncertainty and \
                decisions needed from the parent. Keep handoff summaries and \
                agent_report messages within 4 KiB; put detailed findings in a report \
                file when permitted. Do not include raw logs or code dumps in handoffs. \
                The parent-facing transport caps previews; oversized final answers are \
                saved to a persistent report when workspace storage is available. The \
                full transcript remains \
                local.</child_report_policy>\n</otto_system_prompt>",
            (self.config.prompt_for)(&tools.definitions()),
            crate::skill::prompt::escape_html(role_body),
        ));

        let template = &self.config.template;
        let now_clock = Arc::clone(&template.now);
        let new_id = Arc::clone(&template.new_id);
        let new_operation_id = Arc::clone(&template.new_operation_id);
        let child_inbox = self
            .config
            .tasks
            .child_inbox(&task.id)
            .unwrap_or_else(|| Arc::new(Inbox::new(None)) as Arc<dyn Mailbox>);
        // The transcript file stem (`<task-id>-<child-id>`), never the full
        // path, which contains the home directory. Empty for memory sessions.
        let cache_key = self
            .config
            .tasks
            .get(&task.id)
            .and_then(|t| {
                std::path::Path::new(&t.session_path)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let options = Options {
            diagnostics: template
                .diagnostics
                .as_ref()
                .and_then(|sink| sink.for_task(&task.id)),
            hooks: template
                .plugins
                .as_ref()
                .filter(|p| p.has_hooks())
                .map(|p| {
                    let child_history_path = self
                        .config
                        .tasks
                        .get(&task.id)
                        .map(|t| t.session_path)
                        .unwrap_or_default();
                    p.bind(
                        crate::plugin::hooks::SessionBinding {
                            session_id: format!(
                                "{}:{}",
                                self.config
                                    .approval
                                    .as_ref()
                                    .map_or("child", |(_, s)| s.session_id.as_str()),
                                task.id
                            ),
                            parent_session_id: self
                                .config
                                .approval
                                .as_ref()
                                .map(|(_, s)| s.session_id.clone()),
                            task_id: task.id.clone(),
                            mode: if transcript.messages().is_empty() {
                                "new"
                            } else {
                                "resume"
                            }
                            .into(),
                            headless: false,
                            history_path: Arc::new(move || child_history_path.clone()),
                        },
                        template.hook_guard.clone(),
                    )
                }),
            model: redactor.redact_string(&model),
            provider_name: template.provider_name.clone(),
            system_prompt,
            thinking: redactor.redact_string(&template.thinking),
            cache_key,
            now: Box::new(move || now_clock()),
            new_id: Box::new(move || new_id()),
            new_operation_id: Box::new(move || new_operation_id()),
            request_sizer: template.request_sizer.clone(),
            compaction: template.compaction,
            progress: template.progress,
            // A child gets no memory binding, no registry of its own, and a
            // private inbox: it can neither recall, delegate, nor observe the
            // parent's notifications, but the parent can send messages into
            // this private queue with agent_send.
            memory: None,
            mailbox: child_inbox,
            version: env!("CARGO_PKG_VERSION").into(),
            ..Options::default()
        };

        let child = Agent::with_redactor(
            SharedProvider {
                provider: Arc::clone(&self.config.provider),
                timeout: template.provider_timeout,
                cancellation_grace: template.cancellation_grace,
            },
            tools,
            SharedTranscript(Arc::clone(&transcript)),
            options,
            self.redactor(),
        );

        let runner = Arc::clone(self);
        let task_id = task.id.clone();
        let prompt = request.prompt;
        // Acquire before spawning: even a queued child must outlive the parent's
        // turn lease. The closure releases this on success, error or cancellation.
        let output_lease = self.output_logs.get().map(|logs| logs.lease());
        tokio::spawn(async move {
            let _output_lease = output_lease;
            let _scratch = scratch;
            runner
                .run_child(ChildRun {
                    cancel,
                    task_id,
                    prompt,
                    child,
                    transcript,
                    snapshot,
                    deadline,
                })
                .await;
        });
        start_guard.disarm();

        Ok(task)
    }

    pub(super) fn redactor(&self) -> Redactor {
        Redactor::with_completeness(
            &self.config.redaction_values,
            self.config.redaction_complete,
        )
    }

    /// Replays the inherited snapshot, waits for a semaphore slot or for
    /// cancellation while queued, runs the child to completion, and always
    /// finishes the task record.
    async fn run_child(&self, run: ChildRun) {
        let ChildRun {
            cancel,
            task_id,
            prompt,
            child,
            transcript,
            snapshot,
            deadline,
        } = run;
        for message in snapshot {
            if let Err(error) = transcript.append(message).await {
                cancel.cancel();
                let messages = transcript.messages();
                self.finish(
                    &task_id,
                    TaskStatus::Failed,
                    self.now(),
                    &messages,
                    Some(&error.to_string()),
                    Some(&transcript),
                )
                .await;
                return;
            }
        }

        let permit = tokio::select! {
            biased;
            () = deadline.expired() => {
                let messages = transcript.messages();
                self.finish(&task_id, TaskStatus::Interrupted, self.now(), &messages, Some("task deadline exceeded"), Some(&transcript)).await;
                return;
            }
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit,
            () = cancel.cancelled() => {
                let messages = transcript.messages();
                self.finish(&task_id, TaskStatus::Canceled, self.now(), &messages, None, Some(&transcript)).await;
                return;
            }
        };

        self.config.tasks.mark_running(&task_id, self.now());

        let mut progress = ChildProgress::new(Arc::clone(&self.config.tasks), task_id.clone());
        let usage = self
            .config
            .usage
            .as_ref()
            .map(|collector| collector.for_task(&task_id));
        let control = Control::new(deadline);
        let child_inbox = self.config.tasks.child_inbox(&task_id);
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let outcome = {
            let mut handle = |event: Event| {
                if let Some(usage) = &usage {
                    let _ = usage.record(&event);
                }
                progress.handle(event);
            };
            let sink: EventSink<'_> = &mut handle;
            let run = async {
                let mut outcome = child.run_with_control(&prompt, &mut *sink, &control).await;
                // A parent message queued while the last provider call was in
                // flight is not delivered by that turn; an empty-text run
                // delivers whatever is still queued.
                while outcome.is_ok()
                    && child_inbox.as_ref().is_some_and(|inbox| !inbox.is_empty())
                    && control.stop_reason().is_none()
                    && !cancel.is_cancelled()
                {
                    outcome = child.run_with_control("", &mut *sink, &control).await;
                }
                outcome
            };
            tokio::pin!(run);
            tokio::select! {
                biased;
                outcome = &mut run => outcome,
                () = cancel.cancelled() => {
                    control.stop(OperationStopReason::UserCancellation);
                    run.await
                }
                () = deadline.expired() => {
                    control.stop(OperationStopReason::Deadline);
                    // Deadline requests cooperative stop; the effectful child
                    // remains owned here until it safely settles.
                    run.await
                }
            }
        };
        drop(permit);
        child.end_hooks().await;
        let _ = child.close();

        let status = match &outcome {
            _ if control.stop_reason() == Some(OperationStopReason::Deadline) => {
                TaskStatus::Interrupted
            }
            Ok(()) => TaskStatus::Succeeded,
            Err(_) if cancel.is_cancelled() => TaskStatus::Canceled,
            Err(_) => TaskStatus::Failed,
        };
        let error = if status == TaskStatus::Interrupted {
            Some("task deadline exceeded".to_string())
        } else {
            outcome.err().map(|error| error.to_string())
        };
        let messages = transcript.messages();
        self.finish(
            &task_id,
            status,
            self.now(),
            &messages,
            error.as_deref(),
            Some(&transcript),
        )
        .await;
    }

    /// Marks a task final and pushes its completion notification. The
    /// notification is pushed before `otto.task_result` is appended to
    /// `transcript` (`None` when the task never got one, such as when the
    /// child-session builder itself failed), and before the registry update
    /// that releases [`Tasks::wait`], so a caller unblocked by a wait always
    /// finds the notification already in the inbox.
    pub(super) async fn finish(
        &self,
        task_id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        messages: &[Message],
        run_error: Option<&str>,
        transcript: Option<&Transcript>,
    ) {
        let (result, error_text, migrating) =
            self.prepare_finish(task_id, status, finished_at, messages, run_error);

        if let Some(transcript) = transcript {
            let result_status = if migrating {
                crate::subagent::interrupted::INTERRUPTED_STATUS
            } else {
                status.as_str()
            };
            let _ = append_task_custom(
                transcript,
                TASK_RESULT_CUSTOM_TYPE,
                &TaskResultData {
                    status: result_status,
                    error: &error_text,
                },
            )
            .await;
        }
        self.config
            .tasks
            .finish(task_id, status, finished_at, &result, &error_text);
    }

    pub(super) fn finish_without_transcript(
        &self,
        task_id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        messages: &[Message],
        run_error: Option<&str>,
    ) {
        let (result, error_text, _) =
            self.prepare_finish(task_id, status, finished_at, messages, run_error);
        self.config
            .tasks
            .finish(task_id, status, finished_at, &result, &error_text);
    }

    fn prepare_finish(
        &self,
        task_id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        messages: &[Message],
        run_error: Option<&str>,
    ) -> (String, String, bool) {
        let mut result = last_assistant_text(messages);
        if result.is_empty() {
            result = "(sub-agent returned no final text)".to_string();
        }
        let error_text = match (status, run_error) {
            (TaskStatus::Failed | TaskStatus::Interrupted, Some(error)) => error.to_string(),
            _ => String::new(),
        };

        let mut final_task = self.config.tasks.get(task_id).unwrap_or_default();
        final_task.status = status;
        final_task.finished_at = Some(finished_at);
        final_task.result = result.clone();
        final_task.error = error_text.clone();

        // A task cancelled by `Tasks::begin_migration` moved because its
        // process is leaving, not because it failed or was asked to stop:
        // skip the ordinary completion notification (`notify_moved` sends
        // one notification for the whole migration instead) and record the
        // transcript's own result as "interrupted" rather than "canceled",
        // matching how a takeover-side scan would classify it.
        let migrating = status == TaskStatus::Canceled && self.config.tasks.is_migrating();
        if !migrating {
            self.config.tasks.notifications().push(Notification {
                task_id: task_id.to_string(),
                kind: Some(NotificationKind::TaskFinished),
                text: self.parent_text(&final_task, &completion_text(&final_task, usize::MAX)),
                usage: final_task.usage_present.then_some(final_task.usage),
                // The parent's footer adds this task's tokens to the session
                // totals; it needs to know whether the cached share behind
                // them is measured or merely unreported.
                cached_input_tokens_present: final_task.cached_input_tokens_present,
                ..Default::default()
            });
        }
        (result, error_text, migrating)
    }
}

/// Encodes `data` and appends it to `transcript` as a `custom` entry typed
/// `custom_type`. A failure to encode or append is dropped, the same way
/// [`Recorder::upsert`](crate::subagent::record::Recorder::upsert) drops a
/// task record write failure: in TUI mode a stderr write from a background
/// task is drawn over the screen. The task's completion notification to the
/// parent does not depend on these entries.
async fn append_task_custom(
    transcript: &Transcript,
    custom_type: &str,
    data: &impl Serialize,
) -> Result<(), otto_core::session::SessionError> {
    let json = serde_json::to_string(data)
        .map_err(|error| otto_core::session::SessionError::Persist(error.to_string()))?;
    transcript.append_custom(custom_type, &json).await
}

/// Turns a child's events into task record updates. It never forwards an event
/// to the parent's frontend.
struct ChildProgress {
    tasks: Arc<Tasks>,
    task_id: String,
    text: String,
}

impl ChildProgress {
    fn new(tasks: Arc<Tasks>, task_id: String) -> Self {
        Self {
            tasks,
            task_id,
            text: String::new(),
        }
    }

    fn handle(&mut self, event: Event) {
        self.tasks.observe(&self.task_id, &event);
        match event {
            Event::TextDelta { text } => self.text.push_str(&text),
            Event::ProviderRetry {
                interrupted_output: true,
                ..
            } => self.text.clear(),
            Event::ProviderUsage {
                usage,
                present,
                cached_input_tokens_present,
            } => {
                let text = cap_last_bytes(&self.text, 500).to_string();
                self.text.clear();
                self.tasks.record_provider_step(
                    &self.task_id,
                    usage,
                    &text,
                    present,
                    cached_input_tokens_present,
                );
            }
            Event::ToolCallFinished { .. } => self.tasks.record_tool_finished(&self.task_id),
            Event::ToolCallStarted {
                tool_name,
                arguments,
                ..
            } => {
                let preview = compact_args_preview(&arguments, 60);
                let last_tool = if preview.is_empty() {
                    tool_name
                } else {
                    format!("{tool_name} {preview}")
                };
                self.tasks.record_tool_call(&self.task_id, &last_tool);
            }
            _ => {}
        }
    }
}
