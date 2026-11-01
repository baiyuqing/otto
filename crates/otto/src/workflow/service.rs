//! Workflow definitions and durable run orchestration.
#[cfg(test)]
use super::Store;
use super::contracts::*;
use super::error::Error;
use super::repository::WorkflowStore;
use crate::deadline::{Control, Deadline};
use crate::log::Logger;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
const MAX_DEFINITION_BYTES: usize = 1 << 20;
const MAX_DESCRIPTION_CHARS: usize = 1024;
const MAX_STEPS: usize = 32;

#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String>;

    async fn close(&self) {}
}

struct ActiveRun {
    cancel: CancellationToken,
    done: Arc<Notify>,
    relaunch_requested: bool,
}

#[async_trait::async_trait]
pub(crate) trait Delivery: Send + Sync {
    fn stop(&self);
    fn is_stopped(&self) -> bool;
    async fn join(&self);
}

pub struct Controller {
    deliveries: Mutex<Option<Vec<Arc<dyn Delivery>>>>,
    pub(crate) approval_service: Mutex<Option<std::sync::Weak<crate::approval::Service>>>,
    pub(crate) store: Arc<dyn WorkflowStore>,
    catalog: Catalog,
    executor: Arc<dyn Executor>,
    pub(crate) workspace: String,
    transcript_root: PathBuf,
    runtime: RuntimeIdentity,
    semaphore: Arc<Semaphore>,
    deadlines: DeadlinePolicy,
    logger: Arc<Logger>,
    active: Mutex<HashMap<String, ActiveRun>>,
    guard: Mutex<Option<Box<dyn Send>>>,
}

impl Controller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn WorkflowStore>,
        catalog: Catalog,
        executor: Arc<dyn Executor>,
        workspace: String,
        transcript_root: PathBuf,
        runtime: RuntimeIdentity,
        max_parallel: usize,
    ) -> Arc<Self> {
        // Tests and embedders that have no process log write no events.
        Self::new_with_deadlines(
            store,
            catalog,
            executor,
            workspace,
            transcript_root,
            runtime,
            max_parallel,
            DeadlinePolicy::default(),
            Arc::new(Logger::discard()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_deadlines(
        store: Arc<dyn WorkflowStore>,
        catalog: Catalog,
        executor: Arc<dyn Executor>,
        workspace: String,
        transcript_root: PathBuf,
        runtime: RuntimeIdentity,
        max_parallel: usize,
        deadlines: DeadlinePolicy,
        logger: Arc<Logger>,
    ) -> Arc<Self> {
        Arc::new(Self {
            deliveries: Mutex::new(Some(Vec::new())),
            approval_service: Mutex::new(None),
            store,
            catalog,
            executor,
            workspace,
            transcript_root,
            runtime,
            semaphore: Arc::new(Semaphore::new(max_parallel.max(1))),
            deadlines,
            logger,
            active: Mutex::new(HashMap::new()),
            guard: Mutex::new(None),
        })
    }

    /// Construction-time catalog, including invalid-definition diagnostics.
    pub fn catalog(&self) -> (&[Definition], &[String]) {
        (&self.catalog.definitions, &self.catalog.diagnostics)
    }

    pub fn inspect(&self, name: &str) -> Result<Definition, Error> {
        self.catalog.lookup(name).cloned().ok_or_else(|| Error::not_found("workflow not found; use workflow_list and restart the owner after editing definitions"))
    }

    pub fn set_guard(&self, guard: Box<dyn Send>) {
        *self
            .guard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(guard);
    }

    pub async fn start(self: &Arc<Self>, workflow: &str, input: &str) -> Result<Run, Error> {
        super::provenance::admit_start()?;
        let definition = self
            .catalog
            .lookup(workflow)
            .ok_or_else(|| Error::not_found(format!("workflow {workflow:?} not found")))?;
        validate_write_coordination(definition)?;
        let id = random_id().map_err(|_| Error::internal("generate workflow id failed"))?;
        let run = self.store.create_run(
            &id,
            definition,
            &self.workspace,
            &self.runtime.profile,
            &self.runtime.provider,
            &self.runtime.model,
            input,
        )?;
        self.launch(id)?;
        Ok(run)
    }

    /// Admits only a trusted parent binding; the workflow executor has no binding.
    pub(crate) async fn start_parent(
        self: &Arc<Self>,
        workflow: &str,
        input: &str,
        session: &str,
        operation: &str,
    ) -> Result<Run, Error> {
        super::provenance::admit_start()?;
        if let Some(run) = self.store.origin_run(&self.workspace, session, operation)? {
            if run.workflow != workflow || run.input != input {
                return Err(Error::conflict(
                    "operation receipt conflicts with workflow/input; inspect original run instead of replaying",
                ));
            }
            return Ok(run);
        }
        let definition = self
            .catalog
            .lookup(workflow)
            .ok_or_else(|| Error::not_found("workflow not found; use workflow_list"))?;
        validate_write_coordination(definition)?;
        let id = random_id().map_err(|_| Error::internal("generate workflow id failed"))?;
        let (run, created) = self.store.create_origin_run(
            &id,
            definition,
            &self.workspace,
            &self.runtime,
            input,
            session,
            operation,
        )?;
        // Reconciliation must never relaunch an existing run after uncertain effects.
        if created {
            self.launch(run.id.clone())?;
        }
        Ok(run)
    }

    pub fn get(&self, run_id: &str) -> Result<Run, Error> {
        let run = self
            .store
            .get_run(run_id)?
            .ok_or_else(|| Error::not_found("workflow run not found"))?;
        if run.workspace != self.workspace {
            return Err(Error::not_found("workflow run not found"));
        }
        Ok(run)
    }

    pub fn list(&self) -> Result<Vec<Run>, Error> {
        self.store.list_runs(&self.workspace, &ListQuery::all())
    }

    /// One bounded page of runs, newest first, with the cursor for the next
    /// page when more remain.
    pub fn page(&self, limit: usize, before: Option<String>) -> Result<RunPage, Error> {
        let limit = limit.clamp(1, MAX_PAGE_LIMIT);
        let mut runs = self
            .store
            .list_runs(&self.workspace, &ListQuery::page(limit + 1, before))?;
        let next_before = if runs.len() > limit {
            runs.truncate(limit);
            runs.last()
                .map(|run| run.created_at.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok(RunPage { runs, next_before })
    }

    pub fn requests(&self, run_id: &str) -> Result<Vec<ApprovalRequest>, Error> {
        self.get(run_id)?;
        self.store.requests(run_id)
    }

    pub fn events(&self, run_id: &str, after: i64) -> Result<Vec<EventRecord>, Error> {
        self.get(run_id)?;
        self.store.events(run_id, after)
    }

    pub async fn wait(&self, run_id: &str) -> Result<Run, Error> {
        let done = self
            .active
            .lock()
            .map_err(|_| runtime_unavailable())?
            .get(run_id)
            .map(|active| Arc::clone(&active.done));
        if let Some(done) = done {
            done.notified().await;
        }
        self.get(run_id)
    }

    pub async fn resume(
        self: &Arc<Self>,
        run_id: &str,
        retry_step: Option<&str>,
    ) -> Result<Run, Error> {
        super::provenance::admit_start()?;
        let run = self.get(run_id)?;
        if run.profile != self.runtime.profile
            || run.provider != self.runtime.provider
            || run.model != self.runtime.model
        {
            return Err(Error::conflict(
                "workflow runtime no longer matches the stored run",
            ));
        }
        if let Some(step_id) = retry_step {
            self.store.retry(run_id, step_id)?;
        } else if run.status == RunStatus::Paused {
            return Err(Error::conflict(
                "a paused or failed step must be selected for retry",
            ));
        }
        let current = self.get(run_id)?;
        if current.status == RunStatus::Running {
            self.launch(run_id.to_string())?;
        }
        Ok(current)
    }

    pub async fn fork(self: &Arc<Self>, run_id: &str, after_step: &str) -> Result<Run, Error> {
        super::provenance::admit_start()?;
        let source = self.get(run_id)?;
        if source.profile != self.runtime.profile
            || source.provider != self.runtime.provider
            || source.model != self.runtime.model
        {
            return Err(Error::conflict(
                "workflow runtime no longer matches the stored run",
            ));
        }
        let id = random_id().map_err(|_| Error::internal("generate workflow id failed"))?;
        let fork = self.store.fork_run(&id, run_id, after_step)?;
        if fork.status == RunStatus::Running {
            self.launch(id)?;
        }
        Ok(fork)
    }

    pub fn respond(self: &Arc<Self>, request_id: &str, approved: bool) -> Result<Run, Error> {
        let service = self
            .approval_service
            .lock()
            .map_err(|_| runtime_unavailable())?
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(service) = service {
            let domain = self.approval_request(request_id)?;
            let request = crate::approval::workflow::publish(&service, &self.workspace, &domain)
                .map_err(approval_failure)?;
            service
                .decide(
                    &request.id,
                    &request.scope,
                    request.revision,
                    approved,
                    "",
                    &crate::approval::Actor::local("workflow-command"),
                )
                .map_err(approval_failure)?;
            service
                .apply_available(&self.workspace)
                .map_err(approval_failure)?;
            return self.get(&domain.run_id);
        }
        self.respond_authorized(request_id, approved)
    }

    pub(crate) fn respond_authorized(
        self: &Arc<Self>,
        request_id: &str,
        approved: bool,
    ) -> Result<Run, Error> {
        let request = self
            .store
            .request(request_id)?
            .ok_or_else(|| Error::not_found("approval request not found"))?;
        self.get(&request.run_id)?;
        if !approved {
            // Rejecting a gate cancels the run, so its in-flight attempts are
            // stopped before the store records the cancellation; otherwise a
            // sibling agent keeps working after the human said no.
            if let Ok(active) = self.active.lock()
                && let Some(active) = active.get(&request.run_id)
            {
                active.cancel.cancel();
            }
        }
        let run_id = self.store.respond(request_id, approved)?;
        let run = self.get(&run_id)?;
        if approved && run.status == RunStatus::Running {
            self.launch(run_id)?;
        } else {
            self.log_run_if_final(&run);
        }
        Ok(run)
    }

    pub async fn cancel(&self, run_id: &str) -> Result<Run, Error> {
        let active = self
            .active
            .lock()
            .map_err(|_| runtime_unavailable())?
            .get(run_id)
            .map(|active| (active.cancel.clone(), Arc::clone(&active.done)));
        if let Some((cancel, done)) = active {
            cancel.cancel();
            done.notified().await;
        }
        let before = self.get(run_id)?.status;
        self.store.cancel_run(run_id)?;
        let run = self.get(run_id)?;
        if !is_final(before) {
            self.log_run_if_final(&run);
        }
        Ok(run)
    }

    /// Writes `workflow_run_finished` when `run` has a final status.
    fn log_run_if_final(&self, run: &Run) {
        if !is_final(run.status) {
            return;
        }
        let mut fields = vec![
            ("run_id", run.id.clone()),
            ("workflow", run.workflow.clone()),
            ("status", run.status.as_str().to_string()),
        ];
        if let Some(duration) = self.store.run_age_ms(&run.id) {
            fields.push(("duration_ms", duration.to_string()));
        }
        self.logger.info("workflow_run_finished", &fields);
    }

    fn log_step_finished(&self, run: &Run, step_id: &str, status: &str, started: Instant) {
        self.logger.info(
            "workflow_step_finished",
            &[
                ("run_id", run.id.clone()),
                ("workflow", run.workflow.clone()),
                ("step_id", step_id.to_string()),
                ("status", status.to_string()),
                ("duration_ms", started.elapsed().as_millis().to_string()),
            ],
        );
    }

    /// The number of runs currently executing. Used by `otto_cli::serve`'s
    /// workspace removal to refuse unloading a workspace with work in
    /// flight.
    pub fn active_runs(&self) -> usize {
        self.active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }

    pub(crate) fn register_delivery(&self, binding: Arc<dyn Delivery>) {
        let mut deliveries = self.deliveries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(deliveries) = deliveries.as_mut() {
            // A closed runner stops its binding; drop it so the list stays bounded.
            deliveries.retain(|registered| !registered.is_stopped());
            deliveries.push(binding);
        } else {
            binding.stop();
        }
    }
    pub async fn close(&self) {
        let deliveries = self
            .deliveries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_default();
        for binding in &deliveries {
            binding.stop();
        }
        for binding in deliveries {
            binding.join().await;
        }
        let active: Vec<(String, CancellationToken, Arc<Notify>)> = self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .map(|(run_id, active)| {
                (
                    run_id.clone(),
                    active.cancel.clone(),
                    Arc::clone(&active.done),
                )
            })
            .collect();
        for (_, cancel, _) in &active {
            cancel.cancel();
        }
        for (run_id, _, done) in active {
            done.notified().await;
            let _ = self.store.pause_run(&run_id, "workflow runtime closed");
        }
        self.executor.close().await;
    }

    fn launch(self: &Arc<Self>, run_id: String) -> Result<(), Error> {
        let cancel = CancellationToken::new();
        let done = Arc::new(Notify::new());
        {
            let mut active = self.active.lock().map_err(|_| runtime_unavailable())?;
            if let Some(active) = active.get_mut(&run_id) {
                active.relaunch_requested = true;
                return Ok(());
            }
            active.insert(
                run_id.clone(),
                ActiveRun {
                    cancel: cancel.clone(),
                    done: Arc::clone(&done),
                    relaunch_requested: false,
                },
            );
        }
        let controller = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = controller.drive(&run_id, &cancel).await {
                // A paused run must explain itself: the cause goes to the log and
                // onto the interrupted attempts it stopped.
                let reason = error.to_string();
                controller.logger.error(
                    "workflow_run_paused",
                    &[("run_id", run_id.clone()), ("reason", reason.clone())],
                );
                let _ = controller.store.pause_run(&run_id, &reason);
            }
            let active = controller
                .active
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&run_id);
            let relaunch = !cancel.is_cancelled()
                && (active.is_some_and(|active| active.relaunch_requested)
                    || controller.get(&run_id).is_ok_and(|run| {
                        run.status == RunStatus::Running
                            && run
                                .steps
                                .iter()
                                .any(|step| step.status == StepStatus::Ready)
                    }));
            if relaunch {
                let _ = controller.launch(run_id.clone());
            }
            done.notify_one();
        });
        Ok(())
    }
}

fn is_final(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Canceled
    )
}

/// Assembles one attempt prompt. Dependency results share whatever budget the
/// step prompt and workflow input leave, so a large upstream result truncates
/// with a marker instead of failing after the attempt is already claimed.
fn attempt_prompt(run: &Run, step: &StepRecord) -> Result<String, Error> {
    let mut prompt = step.prompt.clone();
    if !run.input.is_empty() {
        prompt.push_str("\n\n## Workflow input\n");
        prompt.push_str(&run.input);
    }
    if step.kind == StepKind::Handoff {
        prompt.push_str("\n\n## Handoff\nYou are receiving control from: ");
        prompt.push_str(&step.needs.join(", "));
    }
    if prompt.len() > MAX_TEXT_BYTES {
        return Err(Error::invalid(
            "workflow step prompt and input exceed 65536 UTF-8 bytes",
        ));
    }
    for (index, dependency) in step.needs.iter().enumerate() {
        let result = run
            .steps
            .iter()
            .find(|candidate| &candidate.id == dependency)
            .filter(|candidate| candidate.status == StepStatus::Succeeded)
            .map(|candidate| candidate.result.as_str())
            .ok_or_else(|| {
                Error::conflict(format!("workflow dependency {dependency:?} has no result"))
            })?;
        prompt.push_str("\n\n## Result from ");
        prompt.push_str(dependency);
        prompt.push('\n');
        // Split what is left evenly over the dependencies still to come.
        let budget = MAX_TEXT_BYTES.saturating_sub(prompt.len()) / (step.needs.len() - index);
        prompt.push_str(&excerpt(result, budget));
    }
    Ok(prompt)
}

/// Returns at most `budget` bytes of `result`, marking a cut when it happens.
fn excerpt(result: &str, budget: usize) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    const MARKER: &str = "\n[truncated]";
    if result.len() <= budget {
        return Cow::Borrowed(result);
    }
    let keep = if budget > MARKER.len() {
        budget - MARKER.len()
    } else {
        budget
    };
    let mut end = keep.min(result.len());
    while end > 0 && !result.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = String::with_capacity(end + MARKER.len());
    text.push_str(&result[..end]);
    if budget > MARKER.len() {
        text.push_str(MARKER);
    }
    Cow::Owned(text)
}

fn runtime_unavailable() -> Error {
    Error::unavailable("workflow runtime unavailable")
}

fn approval_failure(error: crate::approval::Error) -> Error {
    match error {
        crate::approval::Error::Unavailable => {
            Error::unavailable("workflow approval service unavailable")
        }
        crate::approval::Error::CommitUnknown => {
            Error::unavailable("workflow approval commit is unknown; reconcile before retrying")
        }
        crate::approval::Error::NotFound => Error::not_found("approval request not found"),
        crate::approval::Error::Unauthorized => {
            Error::conflict("workflow approval is not permitted in this workspace")
        }
        crate::approval::Error::Conflict => {
            Error::conflict("workflow approval changed or was already decided")
        }
        crate::approval::Error::Invalid(message) => Error::invalid(message),
    }
}

fn random_id() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn secure_directory(path: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(path)
        .map_err(|_| Error::unavailable("create workflow transcript directory failed"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| Error::unavailable("secure workflow transcript directory failed"))
}

mod definition;
mod drive;

pub use definition::*;

#[cfg(test)]
mod tests;
