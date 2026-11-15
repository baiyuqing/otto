//! Recovered snapshots and atomic explicit resume admission.

use super::*;

impl Tasks {
    pub(crate) fn with_admission_transcript(
        mut self,
        transcript: Arc<dyn crate::session::SessionHandle>,
    ) -> Self {
        self.admission_transcript = Some(transcript);
        self
    }

    /// Admission is durable before construction or execution. A receipt without
    /// a usable child spec remains visible but cannot authorize a resume.
    pub(crate) async fn persist_admission(&self, task: &Task) -> Result<(), TaskError> {
        let Some(transcript) = &self.admission_transcript else {
            return Ok(());
        };
        let data = serde_json::json!({"version": 1, "id": task.id, "name": task.name,
            "description": task.description, "agent": task.agent, "model": task.model,
            "context": task.context});
        transcript
            .append_custom("otto.task_admitted", &data.to_string())
            .await
            .map_err(|error| TaskError::NotPersisted(error.to_string()))
    }

    /// Imports durable state without attaching execution, history or cancellation.
    /// Recovery must never make a historical owner look like this process.
    pub(crate) fn import_recovered(&self, task: Task) -> Result<(), TaskError> {
        if !task.is_final() {
            return Err(TaskError::Invalid(
                "recovery requires a non-running snapshot".into(),
            ));
        }
        let mut state = self.lock();
        if state.closed {
            return Err(TaskError::Closed);
        }
        if state.entries.contains_key(&task.id) {
            return Err(TaskError::Invalid(format!(
                "duplicate recovered task {}",
                task.id
            )));
        }
        if !task.name.is_empty() && state.names.contains_key(&task.name) {
            return Err(TaskError::Invalid(format!(
                "duplicate recovered name {}",
                task.name
            )));
        }
        let counter = task
            .id
            .strip_prefix('t')
            .and_then(|id| id.parse::<u64>().ok())
            .ok_or_else(|| TaskError::Invalid("invalid recovered task id".into()))?;
        state.counter = state.counter.max(counter);
        if !task.name.is_empty() {
            state.names.insert(task.name.clone(), task.id.clone());
        }
        state.order.push(task.id.clone());
        state.entries.insert(
            task.id.clone(),
            Entry {
                task,
                in_tool: false,
                observer: None,
                cancel: None,
                history: None,
                inbox: Arc::new(Inbox::new(None)),
                reports_left: Arc::new(AtomicUsize::new(REPORT_BUDGET)),
                done: Arc::new(Notify::new()),
            },
        );
        drop(state);
        self.signal();
        // Do not mirror imported history with the current process's owner fields.
        Ok(())
    }

    /// Atomically claims an interrupted task for one explicit resume. Ordinary
    /// admission retains its duplicate checks. The transcript path and logical
    /// identity are immutable across this transition; fresh controls replace old ones.
    pub(crate) fn begin_resume(
        &self,
        id: &str,
        expected_attempt: &str,
        mut task: Task,
        cancel: CancellationToken,
        history: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    ) -> Result<Task, TaskError> {
        let mut state = self.lock();
        if state.closed {
            return Err(TaskError::Closed);
        }
        let entry = state
            .entries
            .get_mut(id)
            .ok_or_else(|| TaskError::NotFound(id.into()))?;
        if entry.task.status != TaskStatus::Interrupted || entry.task.attempt_id != expected_attempt
        {
            return Err(TaskError::Invalid(format!(
                "task {id} is not interrupted or is already resuming"
            )));
        }
        task.attempt_id = entry.task.attempt_id.clone();
        task.id = entry.task.id.clone();
        task.name = entry.task.name.clone();
        task.session_path = entry.task.session_path.clone();
        task.status = TaskStatus::Queued;
        task.created_at = entry.task.created_at.or(task.created_at);
        *entry = Entry {
            task: task.clone(),
            in_tool: false,
            observer: Some(observer::Projection::default()),
            cancel: Some(cancel),
            history,
            inbox: Arc::new(Inbox::new(None)),
            reports_left: Arc::new(AtomicUsize::new(REPORT_BUDGET)),
            done: Arc::new(Notify::new()),
        };
        drop(state);
        self.signal();
        self.record(id);
        Ok(task)
    }

    /// Retain readable history without retaining a failed store's writer lock.
    pub(crate) fn freeze_recovery_history(&self, id: &str, messages: Vec<Message>) {
        if let Some(entry) = self.lock().entries.get_mut(id) {
            entry.history = Some(Arc::new(move || messages.clone()));
        }
    }

    pub(crate) fn recovery_writer(&self) -> Option<Arc<dyn crate::session::WriterOwnership>> {
        self.admission_transcript
            .as_ref()
            .and_then(|transcript| transcript.lease())
    }

    pub(crate) fn has_delivered_input(&self, id: &str) -> bool {
        self.admission_transcript
            .as_ref()
            .is_some_and(|transcript| transcript.has_input(id))
    }

    pub(crate) fn set_attempt_id(&self, id: &str, attempt_id: &str) {
        if let Some(entry) = self.lock().entries.get_mut(id) {
            entry.task.attempt_id = attempt_id.to_owned();
        }
    }

    pub(crate) fn set_recovery_coverage(&self, complete: bool, highest_counter: u64) {
        let mut state = self.lock();
        state.recovery_admission_blocked = !complete;
        state.counter = state.counter.max(highest_counter);
    }

    pub(crate) fn set_recovery_diagnostics(&self, diagnostics: Vec<String>) {
        self.lock().recovery_diagnostics = diagnostics;
        self.signal();
    }

    /// Coverage and repair errors remain inspectable even if execution is disabled.
    pub(crate) fn recovery_diagnostics(&self) -> Vec<String> {
        self.lock().recovery_diagnostics.clone()
    }
}
