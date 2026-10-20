//! Task admission: starting, resuming and running a child to completion.

use super::*;

impl Runner {
    /// Registers a task and spawns its child, or leaves it queued when
    /// [`Config::max_parallel`] children are already running. An unknown agent
    /// name, an invalid context, or `inherit` with no parent session is
    /// rejected before any task is created.
    pub async fn start(self: &Arc<Self>, request: StartRequest) -> Result<Task, StartError> {
        self.start_resolved_with_deadline(request, None, None, None)
            .await
    }

    async fn start_resolved_with_deadline(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Option<Transcript>,
        inline_definition: Option<Definition>,
        parent_deadline: Option<Deadline>,
    ) -> Result<Task, StartError> {
        let now = self.now();
        let description = truncate_with_ellipsis(request.description.trim(), MAX_DESCRIPTION_CHARS);

        let agent_name = request.agent.trim();
        let definition: Option<Definition> = if let Some(definition) = inline_definition {
            if agent_name != definition.name {
                return Err(StartError::UnknownAgent(agent_name.to_string()));
            }
            Some(definition)
        } else if agent_name.is_empty() {
            None
        } else {
            Some(
                self.config
                    .catalog
                    .lookup(agent_name)
                    .ok_or_else(|| StartError::UnknownAgent(agent_name.to_string()))?
                    .clone(),
            )
        };

        use super::super::tasks::ModelSource;
        let (model, model_source) = [
            (request.model.trim(), ModelSource::Explicit),
            (
                definition.as_ref().map_or("", |d| d.model.trim()),
                ModelSource::Definition,
            ),
            (
                self.config
                    .template
                    .subagent_models
                    .first()
                    .map_or("", |s| s.trim()),
                ModelSource::ConfiguredFirst,
            ),
            (self.config.template.model.trim(), ModelSource::Parent),
        ]
        .into_iter()
        .find(|(model, _)| !model.is_empty())
        .map(|(model, source)| (model.to_owned(), source))
        .unwrap_or_else(|| (String::new(), ModelSource::Parent));
        let mut context = first_non_empty(&[
            request.context.trim(),
            definition.as_ref().map_or("", |d| d.context.trim()),
            "fresh",
        ]);
        if context != "fresh" && context != "inherit" {
            return Err(StartError::InvalidContext);
        }
        let fork_before = request.fork_before.trim();
        let snapshot = if !fork_before.is_empty() {
            if request.context.trim() == "fresh" {
                return Err(StartError::ForkNeedsInherit);
            }
            let parent_fork = self
                .config
                .parent_fork
                .as_ref()
                .ok_or_else(|| StartError::Fork("needs a saved session".into()))?;
            context = "inherit".to_owned();
            parent_fork(fork_before).map_err(StartError::Fork)?
        } else if context == "inherit" {
            let parent_session = self
                .config
                .parent_session
                .as_ref()
                .ok_or(StartError::InheritUnavailable)?;
            inherit_snapshot(&parent_session()).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.spawn(
            request,
            definition,
            description,
            model,
            model_source,
            context,
            now,
            snapshot,
            transcript.map(|transcript| (transcript, String::new())),
            None,
            self.task_deadline(parent_deadline),
        )
        .await
    }

    fn task_deadline(&self, parent: Option<Deadline>) -> Deadline {
        let parent = parent.unwrap_or_else(Deadline::unlimited);
        self.config
            .template
            .task_timeout
            .map_or(parent, |timeout| Deadline::earlier(parent, timeout))
    }

    /// Resumes an interrupted sub-agent task under its original id. `target`
    /// matches a child transcript's task id first, then its name, among the
    /// current session's children. `prompt` becomes the task's next user
    /// message; the run replays no inherited snapshot, since the existing
    /// transcript already holds the task's history.
    pub async fn resume(
        self: &Arc<Self>,
        target: &str,
        prompt: String,
    ) -> Result<Task, StartError> {
        let locate = self
            .config
            .children_dir
            .as_ref()
            .ok_or(StartError::ResumeNoSession)?;
        let children_dir = locate().ok_or(StartError::ResumeNoSession)?;

        let records = interrupted::scan(&children_dir);
        let record = records
            .iter()
            .find(|record| record.task_id == target)
            .or_else(|| records.iter().find(|record| record.name == target))
            .ok_or_else(|| StartError::ResumeNotFound(target.to_string()))?;
        if record.final_status.as_deref() != Some(interrupted::INTERRUPTED_STATUS) {
            return Err(StartError::ResumeNotInterrupted(record.task_id.clone()));
        }

        let (store, _warnings) = crate::session::Store::open(&record.path)
            .map_err(|error| StartError::ResumeFailed(error.to_string()))?;
        if let Some(lease) = self.config.lease.as_ref().and_then(|source| source()) {
            store.set_lease_check(lease);
        }
        let transcript: Transcript = Arc::new(store);
        let definition = record
            .definition
            .as_ref()
            .map(TaskSpecDefinition::to_definition)
            .transpose()
            .map_err(StartError::ResumeFailed)?;
        let now = self.now();
        let description = truncate_with_ellipsis(record.description.trim(), MAX_DESCRIPTION_CHARS);
        let request = StartRequest {
            prompt,
            description: record.description.clone(),
            name: record.name.clone(),
            model: record.model.clone(),
            agent: record.agent.clone(),
            context: record.context.clone(),
            fork_before: String::new(),
            parent_tool_call_id: String::new(),
        };
        self.spawn(
            request,
            definition,
            description,
            record.model.clone(),
            record.model_source,
            record.context.clone(),
            now,
            Vec::new(),
            Some((transcript, record.path.to_string_lossy().into_owned())),
            Some(record.task_id.clone()),
            self.task_deadline(None),
        )
        .await
    }

    /// Runs using a definition snapshot captured when a durable workflow was
    /// created, so editing `AGENT.md` cannot change a resumed run, while
    /// inheriting the caller's absolute deadline. The child's local task
    /// timeout may only shorten that budget.
    pub async fn run_with_definition_control(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
        definition: Definition,
        control: &Control,
    ) -> Result<Task, String> {
        self.run_resolved(
            request,
            transcript,
            Some(definition),
            control.cancellation_token(),
            Some(control.deadline()),
        )
        .await
    }

    async fn run_resolved(
        self: &Arc<Self>,
        request: StartRequest,
        transcript: Transcript,
        definition: Option<Definition>,
        cancel: &CancellationToken,
        parent_deadline: Option<Deadline>,
    ) -> Result<Task, String> {
        let task = self
            .start_resolved_with_deadline(request, Some(transcript), definition, parent_deadline)
            .await
            .map_err(|error| error.to_string())?;
        let id = task.id.clone();
        let result = match self.config.tasks.wait(&id, cancel).await {
            Ok(task) => Ok(task),
            Err(TaskError::Canceled) => {
                self.config
                    .tasks
                    .cancel(&id)
                    .map_err(|error| error.to_string())?;
                self.config
                    .tasks
                    .wait(&id, &CancellationToken::new())
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        };
        if result.is_ok() {
            self.config.tasks.remove_final(&id);
        }
        result
    }
}
