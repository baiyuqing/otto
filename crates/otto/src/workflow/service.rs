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

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    definitions: Vec<Definition>,
}

impl Catalog {
    #[cfg(test)]
    pub(crate) fn from_definitions(definitions: Vec<Definition>) -> Self {
        Self { definitions }
    }

    pub fn discover(
        roots: &[std::path::PathBuf],
        agents: &crate::subagent::Catalog,
    ) -> (Self, Vec<String>) {
        let mut by_name = std::collections::BTreeMap::new();
        let mut warnings = Vec::new();
        for root in roots {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!("workflows root {}: {error}", root.display()));
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                    continue;
                }
                match entry.file_type() {
                    Ok(file_type) if file_type.is_file() => {}
                    _ => continue,
                }
                let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let data = match read_definition_file(&path) {
                    Ok(data) => data,
                    Err(error) => {
                        warnings.push(format!("workflow {}: {error}", path.display()));
                        continue;
                    }
                };
                match parse_definition(name, &data, &|agent| agents.lookup(agent).is_some()) {
                    Ok(mut definition) => {
                        let names: HashSet<String> = definition
                            .steps
                            .iter()
                            .filter(|step| matches!(step.kind, StepKind::Agent | StepKind::Handoff))
                            .map(|step| step.agent.clone())
                            .collect();
                        definition.agents = names
                            .into_iter()
                            .filter_map(|name| agents.lookup(&name))
                            .map(|agent| AgentSnapshot {
                                name: agent.name.clone(),
                                description: agent.description.clone(),
                                tools: agent.tools.clone(),
                                model: agent.model.clone(),
                                context: agent.context.clone(),
                                write_policy: SnapshotWritePolicy::from_subagent(
                                    agent.write_policy,
                                ),
                                write_paths: agent.write_paths.clone(),
                                body: agent.body.clone(),
                            })
                            .collect();
                        definition
                            .agents
                            .sort_by(|left, right| left.name.cmp(&right.name));
                        if let Err(error) = validate_write_coordination(&definition) {
                            warnings.push(format!("workflow {}: {error}", path.display()));
                            continue;
                        }
                        definition.hash.clear();
                        definition.hash = format!(
                            "{:x}",
                            Sha256::digest(serde_json::to_vec(&definition).unwrap_or_default())
                        );
                        by_name.insert(definition.name.clone(), definition);
                    }
                    Err(error) => warnings.push(format!("workflow {}: {error}", path.display())),
                }
            }
        }
        (
            Self {
                definitions: by_name.into_values().collect(),
            },
            warnings,
        )
    }

    pub fn lookup(&self, name: &str) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.name == name)
    }

    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }
}

pub(crate) fn validate_write_coordination(definition: &Definition) -> Result<(), Error> {
    let agents: HashMap<&str, &AgentSnapshot> = definition
        .agents
        .iter()
        .map(|agent| (agent.name.as_str(), agent))
        .collect();
    for (left_index, left) in definition.steps.iter().enumerate() {
        if left.kind != StepKind::Agent {
            continue;
        }
        let Some(left_agent) = agents.get(left.agent.as_str()) else {
            continue;
        };
        let Some(left_writer) = writer_scope(left_agent) else {
            continue;
        };
        for right in definition.steps.iter().skip(left_index + 1) {
            if right.kind != StepKind::Agent || ordered_by_dependency(definition, left, right) {
                continue;
            }
            let Some(right_agent) = agents.get(right.agent.as_str()) else {
                continue;
            };
            let Some(right_writer) = writer_scope(right_agent) else {
                continue;
            };
            if writer_scopes_conflict(&left_writer, &right_writer) {
                return Err(Error::invalid(format!(
                    "steps {:?} and {:?} may run concurrently and both can write; add a dependency or use disjoint owned_paths write policies",
                    left.id, right.id
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum WriterScope<'a> {
    Global,
    Owned(&'a [String]),
}

fn writer_scope(agent: &AgentSnapshot) -> Option<WriterScope<'_>> {
    if !agent_can_mutate(agent) {
        return None;
    }
    match agent.write_policy {
        SnapshotWritePolicy::ReadOnly | SnapshotWritePolicy::ProposeOnly => None,
        SnapshotWritePolicy::SingleWriter => Some(WriterScope::Global),
        SnapshotWritePolicy::OwnedPaths => Some(WriterScope::Owned(&agent.write_paths)),
    }
}

fn agent_can_mutate(agent: &AgentSnapshot) -> bool {
    agent.tools.as_ref().is_none_or(|tools| {
        tools
            .iter()
            .any(|tool| matches!(tool.as_str(), "write" | "edit"))
    })
}

fn writer_scopes_conflict(left: &WriterScope<'_>, right: &WriterScope<'_>) -> bool {
    match (left, right) {
        (WriterScope::Global, _) | (_, WriterScope::Global) => true,
        (WriterScope::Owned(left), WriterScope::Owned(right)) => left.iter().any(|left| {
            right
                .iter()
                .any(|right| ownership_patterns_overlap(left, right))
        }),
    }
}

fn ownership_patterns_overlap(left: &str, right: &str) -> bool {
    left == right
        || pattern_prefix(left).is_some_and(|prefix| right.starts_with(prefix))
        || pattern_prefix(right).is_some_and(|prefix| left.starts_with(prefix))
}

fn pattern_prefix(pattern: &str) -> Option<&str> {
    pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("/*"))
        .or_else(|| pattern.split_once('*').map(|(prefix, _)| prefix))
}

fn ordered_by_dependency(definition: &Definition, left: &Step, right: &Step) -> bool {
    depends_on(definition, &left.id, &right.id) || depends_on(definition, &right.id, &left.id)
}

fn depends_on(definition: &Definition, step_id: &str, dependency_id: &str) -> bool {
    let by_id: HashMap<&str, &Step> = definition
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect();
    let mut stack = vec![step_id];
    let mut seen = HashSet::new();
    while let Some(current) = stack.pop() {
        let Some(step) = by_id.get(current) else {
            continue;
        };
        for dependency in &step.needs {
            if dependency == dependency_id {
                return true;
            }
            if seen.insert(dependency.as_str()) {
                stack.push(dependency);
            }
        }
    }
    false
}

fn read_definition_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if file.metadata()?.len() > MAX_DEFINITION_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds 1 MiB",
        ));
    }
    let mut data = Vec::new();
    file.by_ref()
        .take((MAX_DEFINITION_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    if data.len() > MAX_DEFINITION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds 1 MiB",
        ));
    }
    Ok(data)
}

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

pub struct Controller {
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

    pub fn set_guard(&self, guard: Box<dyn Send>) {
        *self
            .guard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(guard);
    }

    pub async fn start(self: &Arc<Self>, workflow: &str, input: &str) -> Result<Run, Error> {
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

    /// The number of runs currently executing. Used by `cli::serve`'s
    /// workspace removal to refuse unloading a workspace with work in
    /// flight.
    pub fn active_runs(&self) -> usize {
        self.active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }

    pub async fn close(&self) {
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

    async fn drive(&self, run_id: &str, cancel: &CancellationToken) -> Result<(), Error> {
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let run = self.get(run_id)?;
            if run.status != RunStatus::Running {
                return Ok(());
            }
            let ready: Vec<StepRecord> = run
                .steps
                .iter()
                .filter(|step| step.status == StepStatus::Ready)
                .cloned()
                .collect();
            if ready.is_empty() {
                return Ok(());
            }

            let mut agents = Vec::new();
            for step in ready {
                match step.kind {
                    StepKind::Approval => {
                        let request_id = random_id()
                            .map_err(|_| Error::internal("generate workflow id failed"))?;
                        let request = self.store.request_approval(&request_id, run_id, &step.id)?;
                        if let Some(service) = self
                            .approval_service
                            .lock()
                            .map_err(|_| runtime_unavailable())?
                            .as_ref()
                            .and_then(std::sync::Weak::upgrade)
                        {
                            crate::approval::workflow::publish(&service, &self.workspace, &request)
                                .map_err(approval_failure)?;
                        }
                    }
                    StepKind::Agent | StepKind::Handoff => agents.push(step),
                }
            }
            if agents.is_empty() {
                continue;
            }

            let mut attempts = JoinSet::new();
            for step in agents {
                let permit = Arc::clone(&self.semaphore)
                    .acquire_owned()
                    .await
                    .map_err(|_| runtime_unavailable())?;
                if cancel.is_cancelled() {
                    drop(permit);
                    return Ok(());
                }
                let next_attempt = step.attempt.saturating_add(1);
                let run_root = self.transcript_root.join(run_id);
                let step_root = run_root.join(&step.id);
                let attempt_root = step_root.join(next_attempt.to_string());
                for directory in [&self.transcript_root, &run_root, &step_root, &attempt_root] {
                    secure_directory(directory)?;
                }
                let session_id = random_id()
                    .map_err(|_| Error::internal("generate workflow session id failed"))?;
                let workspace_key = crate::session::workspace_key(Path::new(&run.workspace))
                    .map_err(|_| Error::unavailable("resolve workflow transcript path failed"))?;
                let transcript_path = attempt_root
                    .join(workspace_key)
                    .join(format!("{session_id}.jsonl"))
                    .to_string_lossy()
                    .into_owned();
                let attempt_number = self.store.claim_ready(run_id, &step.id, &transcript_path)?;
                let attempt = Attempt {
                    run_id: run_id.to_string(),
                    step_id: step.id.clone(),
                    agent: step.agent.clone(),
                    agent_definition: run
                        .definition
                        .agents
                        .iter()
                        .find(|definition| definition.name == step.agent)
                        .cloned()
                        .ok_or_else(|| Error::internal("workflow agent snapshot is missing"))?,
                    prompt: attempt_prompt(&run, &step)?,
                    attempt: attempt_number,
                    session_id,
                    transcript_root: attempt_root.to_string_lossy().into_owned(),
                    transcript_path,
                };
                let executor = Arc::clone(&self.executor);
                let parent_cancel = cancel.clone();
                let started = Instant::now();
                let deadlines = self.deadlines;
                attempts.spawn(async move {
                    let deadline = deadlines
                        .step_timeout
                        .map_or_else(Deadline::unlimited, Deadline::after);
                    let control = Control::new(deadline);
                    let execute = executor.execute(attempt.clone(), &control);
                    tokio::pin!(execute);
                    let mut timed_out = false;
                    let result = tokio::select! {
                        biased;
                        result = &mut execute => result,
                        () = parent_cancel.cancelled() => {
                            control.stop(otto_core::model::OperationStopReason::UserCancellation);
                            execute.await
                        }
                        () = deadline.expired() => {
                            timed_out = true;
                            control.stop(otto_core::model::OperationStopReason::Deadline);
                            // Grace is only an observation window. Keep owning
                            // the effectful future until it safely settles.
                            match tokio::time::timeout(deadlines.cancellation_grace, &mut execute).await {
                                Ok(result) => result,
                                Err(_) => execute.await,
                            }
                        }
                    };
                    drop(permit);
                    (
                        attempt,
                        result,
                        parent_cancel.is_cancelled(),
                        timed_out,
                        started,
                    )
                });
            }

            let mut failed = false;
            let mut timed_out = false;
            while let Some(joined) = attempts.join_next().await {
                let (attempt, result, canceled, attempt_timed_out, started) =
                    joined.map_err(|_| Error::internal("workflow attempt task failed"))?;
                if attempt_timed_out {
                    self.store.interrupt_attempt(
                        &attempt.run_id,
                        &attempt.step_id,
                        attempt.attempt,
                        "workflow step deadline exceeded",
                    )?;
                    self.log_step_finished(&run, &attempt.step_id, "interrupted", started);
                    timed_out = true;
                    continue;
                }
                if canceled {
                    self.store.cancel_attempt(
                        &attempt.run_id,
                        &attempt.step_id,
                        attempt.attempt,
                    )?;
                    self.log_step_finished(&run, &attempt.step_id, "canceled", started);
                    continue;
                }
                match result {
                    Ok(result) => {
                        self.store.finish_attempt(
                            &attempt.run_id,
                            &attempt.step_id,
                            attempt.attempt,
                            Ok(&result),
                        )?;
                        self.log_step_finished(&run, &attempt.step_id, "succeeded", started);
                    }
                    Err(error) => {
                        self.store.finish_attempt(
                            &attempt.run_id,
                            &attempt.step_id,
                            attempt.attempt,
                            Err(&error),
                        )?;
                        self.log_step_finished(&run, &attempt.step_id, "failed", started);
                        cancel.cancel();
                        failed = true;
                    }
                }
            }
            // A batch canceled by `cancel` or `close` belongs to that caller: the
            // driver leaves the run status to it. A batch whose own failure
            // canceled its attempts must still settle below.
            if cancel.is_cancelled() && !failed && !timed_out {
                return Ok(());
            }
            // Each settled attempt settles the run in its own transaction; settle
            // once more so a batch that ended in failure or timeout can never leave
            // the run `running` with no owner.
            self.store.settle(run_id)?;
            if timed_out || failed {
                self.log_run_if_final(&self.get(run_id)?);
                return Ok(());
            }
            self.log_run_if_final(&self.get(run_id)?);
        }
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionFile {
    version: u32,
    #[serde(default)]
    description: String,
    steps: Vec<StepFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    id: String,
    #[serde(default)]
    kind: StepKind,
    #[serde(default)]
    agent: String,
    prompt: String,
    #[serde(default)]
    needs: Vec<String>,
}

pub(crate) fn parse_definition(
    name: &str,
    data: &[u8],
    has_agent: &dyn Fn(&str) -> bool,
) -> Result<Definition, Error> {
    if !valid_name(name) {
        return Err(Error::invalid(format!("workflow name {name:?} is invalid")));
    }
    if data.len() > MAX_DEFINITION_BYTES {
        return Err(Error::invalid(format!("workflow {name} exceeds 1 MiB")));
    }
    let text = std::str::from_utf8(data)
        .map_err(|_| Error::invalid(format!("workflow {name} is not UTF-8")))?;
    let file: DefinitionFile = toml::from_str(text)
        .map_err(|error| Error::invalid(format!("workflow {name}: {error}")))?;
    if file.version != 1 {
        return Err(Error::invalid(format!("workflow {name} version must be 1")));
    }
    if file.description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(Error::invalid(format!(
            "workflow {name} description exceeds {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    if !(1..=MAX_STEPS).contains(&file.steps.len()) {
        return Err(Error::invalid(format!(
            "workflow {name} must contain 1 to {MAX_STEPS} steps"
        )));
    }

    let mut seen = HashSet::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for raw in file.steps {
        if !valid_name(&raw.id) {
            return Err(Error::invalid(format!(
                "workflow {name} step id {:?} is invalid",
                raw.id
            )));
        }
        if !seen.insert(raw.id.clone()) {
            return Err(Error::invalid(format!(
                "workflow {name} contains duplicate step {:?}",
                raw.id
            )));
        }
        if raw.prompt.trim().is_empty() || raw.prompt.len() > MAX_TEXT_BYTES {
            return Err(Error::invalid(format!(
                "workflow {name} step {:?} prompt must be 1 to 65536 UTF-8 bytes",
                raw.id
            )));
        }
        match raw.kind {
            StepKind::Agent | StepKind::Handoff => {
                if raw.agent.is_empty() || !has_agent(&raw.agent) {
                    return Err(Error::invalid(format!(
                        "workflow {name} step {:?} references unknown agent {:?}",
                        raw.id, raw.agent
                    )));
                }
                if raw.kind == StepKind::Handoff && raw.needs.is_empty() {
                    return Err(Error::invalid(format!(
                        "workflow {name} handoff step {:?} requires a dependency",
                        raw.id
                    )));
                }
            }
            StepKind::Approval if !raw.agent.is_empty() => {
                return Err(Error::invalid(format!(
                    "workflow {name} approval step {:?} cannot set agent",
                    raw.id
                )));
            }
            StepKind::Approval => {}
        }
        let mut dependencies = HashSet::new();
        if raw
            .needs
            .iter()
            .any(|dependency| dependency == &raw.id || !dependencies.insert(dependency.clone()))
        {
            return Err(Error::invalid(format!(
                "workflow {name} step {:?} has an invalid dependency",
                raw.id
            )));
        }
        steps.push(Step {
            id: raw.id,
            kind: raw.kind,
            agent: raw.agent,
            prompt: raw.prompt,
            needs: raw.needs,
        });
    }
    validate_graph(name, &steps)?;

    Ok(Definition {
        name: name.to_string(),
        description: file.description,
        steps,
        agents: Vec::new(),
        hash: format!("{:x}", Sha256::digest(data)),
    })
}

fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn validate_graph(name: &str, steps: &[Step]) -> Result<(), Error> {
    let indexes: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(index, step)| (step.id.as_str(), index))
        .collect();
    let mut incoming = vec![0usize; steps.len()];
    let mut outgoing = vec![Vec::new(); steps.len()];
    for (index, step) in steps.iter().enumerate() {
        for dependency in &step.needs {
            let Some(&source) = indexes.get(dependency.as_str()) else {
                return Err(Error::invalid(format!(
                    "workflow {name} step {:?} depends on unknown step {:?}",
                    step.id, dependency
                )));
            };
            incoming[index] += 1;
            outgoing[source].push(index);
        }
    }
    let mut ready: VecDeque<usize> = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for &target in &outgoing[index] {
            incoming[target] -= 1;
            if incoming[target] == 0 {
                ready.push_back(target);
            }
        }
    }
    if visited != steps.len() {
        return Err(Error::invalid(format!(
            "workflow {name} contains a dependency cycle"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use otto_core::operation::OperationControl;
    use tokio::sync::Notify;

    fn definition(source: &[u8]) -> Definition {
        let mut definition = parse_definition("flow", source, &|_| true).expect("definition");
        let names: HashSet<String> = definition
            .steps
            .iter()
            .filter(|step| matches!(step.kind, StepKind::Agent | StepKind::Handoff))
            .map(|step| step.agent.clone())
            .collect();
        definition.agents = names
            .into_iter()
            .map(|name| AgentSnapshot {
                name,
                write_policy: SnapshotWritePolicy::ReadOnly,
                ..AgentSnapshot::default()
            })
            .collect();
        definition
    }

    #[test]
    fn definition_accepts_a_bounded_dag() {
        let definition = parse_definition(
            "review-change",
            br#"
version = 1
description = "Review a change"

[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review it"
needs = ["research"]

[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship it?"
needs = ["review"]
"#,
            &|name| matches!(name, "researcher" | "reviewer"),
        )
        .expect("definition");

        assert_eq!(definition.name, "review-change");
        assert_eq!(definition.steps[0].kind, StepKind::Agent);
        assert_eq!(definition.steps[2].kind, StepKind::Approval);
        assert_eq!(definition.steps[2].needs, ["review"]);
        assert_eq!(definition.hash.len(), 64);
    }

    fn agent_snapshot(
        name: &str,
        tools: Option<&[&str]>,
        write_policy: SnapshotWritePolicy,
        write_paths: &[&str],
    ) -> AgentSnapshot {
        AgentSnapshot {
            name: name.to_string(),
            tools: tools.map(|tools| tools.iter().map(|tool| (*tool).to_string()).collect()),
            write_policy,
            write_paths: write_paths.iter().map(|path| (*path).to_string()).collect(),
            ..AgentSnapshot::default()
        }
    }

    #[test]
    fn write_coordination_rejects_parallel_global_writers() {
        let mut definition = definition(
            br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
"#,
        );
        definition.agents = vec![
            agent_snapshot(
                "left",
                Some(&["read", "edit"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
            agent_snapshot(
                "right",
                Some(&["write"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
        ];

        let error = validate_write_coordination(&definition).expect_err("parallel writers fail");

        assert!(error.message().contains("may run concurrently"), "{error}");
    }

    #[test]
    fn write_coordination_allows_readonly_parallel_and_serial_writers() {
        let mut serial = definition(
            br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
needs = ["left"]
[[steps]]
id = "plan"
agent = "planner"
prompt = "plan"
"#,
        );
        serial.agents = vec![
            agent_snapshot(
                "left",
                Some(&["edit"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
            agent_snapshot(
                "right",
                Some(&["write"]),
                SnapshotWritePolicy::SingleWriter,
                &[],
            ),
            agent_snapshot(
                "planner",
                Some(&["read", "edit"]),
                SnapshotWritePolicy::ProposeOnly,
                &[],
            ),
        ];

        validate_write_coordination(&serial)
            .expect("serial writers and propose-only planner are ok");
    }

    #[test]
    fn write_coordination_allows_disjoint_owned_paths() {
        let mut definition = definition(
            br#"
version = 1
[[steps]]
id = "core"
agent = "core"
prompt = "core"
[[steps]]
id = "docs"
agent = "docs"
prompt = "docs"
"#,
        );
        definition.agents = vec![
            agent_snapshot(
                "core",
                Some(&["edit"]),
                SnapshotWritePolicy::OwnedPaths,
                &["crates/otto-core/**"],
            ),
            agent_snapshot(
                "docs",
                Some(&["write"]),
                SnapshotWritePolicy::OwnedPaths,
                &["docs/**"],
            ),
        ];

        validate_write_coordination(&definition).expect("disjoint owned paths are ok");
    }

    #[test]
    fn write_coordination_rejects_overlapping_owned_paths() {
        let mut definition = definition(
            br#"
version = 1
[[steps]]
id = "a"
agent = "a"
prompt = "a"
[[steps]]
id = "b"
agent = "b"
prompt = "b"
"#,
        );
        definition.agents = vec![
            agent_snapshot(
                "a",
                Some(&["edit"]),
                SnapshotWritePolicy::OwnedPaths,
                &["crates/otto/**"],
            ),
            agent_snapshot(
                "b",
                Some(&["write"]),
                SnapshotWritePolicy::OwnedPaths,
                &["crates/otto/src/**"],
            ),
        ];

        let error = validate_write_coordination(&definition).expect_err("overlap fails");

        assert!(error.message().contains("may run concurrently"), "{error}");
    }

    #[test]
    fn definition_accepts_handoff_to_a_known_agent() {
        let definition = parse_definition(
            "handoff-change",
            br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over and review it"
needs = ["research"]
"#,
            &|name| matches!(name, "researcher" | "reviewer"),
        )
        .expect("definition");

        assert_eq!(definition.steps[1].kind, StepKind::Handoff);
        assert_eq!(definition.steps[1].agent, "reviewer");
        assert_eq!(definition.steps[1].needs, ["research"]);
    }

    #[test]
    fn definition_rejects_a_root_handoff() {
        let error = parse_definition(
            "handoff",
            br#"
version = 1
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over"
"#,
            &|_| true,
        )
        .expect_err("root handoff");

        assert_eq!(
            error.message(),
            "workflow handoff handoff step \"review\" requires a dependency"
        );
    }

    #[test]
    fn definition_rejects_cycles_before_execution() {
        let error = parse_definition(
            "cycle",
            br#"
version = 1
[[steps]]
id = "one"
agent = "worker"
prompt = "one"
needs = ["two"]
[[steps]]
id = "two"
agent = "worker"
prompt = "two"
needs = ["one"]
"#,
            &|_| true,
        )
        .expect_err("cycle");

        assert_eq!(
            error.message(),
            "workflow cycle contains a dependency cycle"
        );
    }

    #[test]
    fn definition_read_refuses_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("directory");
        let source = directory.path().join("source.toml");
        let link = directory.path().join("link.toml");
        std::fs::write(&source, "version = 1").expect("write");
        symlink(&source, &link).expect("symlink");
        assert!(read_definition_file(&link).is_err());
    }

    #[test]
    fn store_commits_dependencies_and_terminal_run() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        let run = store
            .create_run(
                "run-1",
                &definition,
                "/workspace",
                "default",
                "openai-compatible",
                "model",
                "input",
            )
            .expect("create");
        assert_eq!(run.steps[0].status, StepStatus::Ready);
        assert_eq!(run.steps[1].status, StepStatus::Pending);

        let attempt = store
            .claim_ready("run-1", "first", "/tmp/first.jsonl")
            .expect("claim");
        store
            .finish_attempt("run-1", "first", attempt, Ok("result one"))
            .expect("finish first");
        let run = store.get_run("run-1").expect("get").expect("run");
        assert_eq!(run.steps[1].status, StepStatus::Ready);

        let attempt = store
            .claim_ready("run-1", "second", "/tmp/second.jsonl")
            .expect("claim second");
        store
            .finish_attempt("run-1", "second", attempt, Ok("done"))
            .expect("finish second");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Succeeded
        );
    }

    #[test]
    fn fork_copies_the_committed_event_boundary_state() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "left"
agent = "worker"
prompt = "left"
[[steps]]
id = "right"
agent = "worker"
prompt = "right"
[[steps]]
id = "join"
agent = "worker"
prompt = "join"
needs = ["left", "right"]
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "input")
            .expect("create");
        let right = store
            .claim_ready("run-1", "right", "/tmp/right.jsonl")
            .expect("claim right");
        store
            .finish_attempt("run-1", "right", right, Ok("right done"))
            .expect("finish right");
        let left = store
            .claim_ready("run-1", "left", "/tmp/left.jsonl")
            .expect("claim left");
        store
            .finish_attempt("run-1", "left", left, Ok("left done"))
            .expect("finish left");

        let fork = store
            .fork_run("fork-1", "run-1", "left")
            .expect("fork after left");

        assert_eq!(fork.status, RunStatus::Running);
        assert_eq!(fork.forked_from_run_id.as_deref(), Some("run-1"));
        assert_eq!(fork.forked_from_step_id.as_deref(), Some("left"));
        assert!(fork.forked_from_event_seq.is_some());
        assert_eq!(fork.steps[0].status, StepStatus::Succeeded);
        assert_eq!(fork.steps[0].result, "left done");
        assert_eq!(fork.steps[0].source_run_id.as_deref(), Some("run-1"));
        assert_eq!(fork.steps[0].source_step_id.as_deref(), Some("left"));
        assert_eq!(fork.steps[0].source_attempt, Some(1));
        assert_eq!(fork.steps[1].status, StepStatus::Succeeded);
        assert_eq!(fork.steps[2].status, StepStatus::Ready);
    }

    #[test]
    fn fork_of_a_terminal_boundary_is_terminal() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        let attempt = store
            .claim_ready("run-1", "work", "/tmp/work.jsonl")
            .expect("claim");
        store
            .finish_attempt("run-1", "work", attempt, Ok("done"))
            .expect("finish");

        let fork = store
            .fork_run("fork-1", "run-1", "work")
            .expect("fork terminal");

        assert_eq!(fork.status, RunStatus::Succeeded);
        assert_eq!(fork.steps[0].status, StepStatus::Succeeded);
    }

    #[test]
    fn fork_requires_a_committed_success_boundary() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");

        assert_eq!(
            store
                .fork_run("fork-1", "run-1", "second")
                .expect_err("pending step")
                .message(),
            "workflow fork boundary is not a committed success"
        );
    }

    #[test]
    fn recovery_pauses_running_attempt_without_retrying_it() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        store
            .claim_ready("run-1", "work", "/tmp/work.jsonl")
            .expect("claim");
        store
            .create_run("run-2", &definition, "/other", "p", "provider", "m", "")
            .expect("other create");
        store
            .claim_ready("run-2", "work", "/tmp/other.jsonl")
            .expect("other claim");

        assert_eq!(store.recover("/w").expect("recover"), 1);
        let run = store.get_run("run-1").expect("get").expect("run");
        assert_eq!(run.status, RunStatus::Paused);
        assert_eq!(run.steps[0].status, StepStatus::Interrupted);
        assert_eq!(run.steps[0].attempt, 1);
        assert_eq!(
            store.get_run("run-2").expect("get").expect("other").status,
            RunStatus::Running
        );
    }

    #[test]
    fn manual_retry_creates_a_new_attempt_and_keeps_the_old_transcript() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        store
            .claim_ready("run-1", "work", "/tmp/attempt-1.jsonl")
            .expect("first");
        store.recover("/w").expect("recover");
        store.retry("run-1", "work").expect("retry");
        assert_eq!(
            store
                .claim_ready("run-1", "work", "/tmp/attempt-2.jsonl")
                .expect("second"),
            2
        );
        assert_eq!(
            store.get_run("run-1").expect("run").unwrap().steps[0].transcript_path,
            "/tmp/attempt-2.jsonl"
        );
    }

    #[test]
    fn approval_response_is_durable_and_idempotent() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship?"
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        let request = store
            .request_approval("request-1", "run-1", "approve")
            .expect("request");
        assert_eq!(request.status, "pending");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Waiting
        );

        store.respond("request-1", true).expect("approve");
        store.respond("request-1", true).expect("same response");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Succeeded
        );
        assert_eq!(
            store
                .respond("request-1", false)
                .expect_err("conflict")
                .message(),
            "approval request already answered differently"
        );
    }

    #[test]
    fn handoff_prompt_marks_the_transfer() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "research"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "review"
needs = ["research"]
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "input")
            .expect("create");
        let attempt = store
            .claim_ready("run-1", "research", "/tmp/research.jsonl")
            .expect("claim");
        store
            .finish_attempt("run-1", "research", attempt, Ok("facts"))
            .expect("finish");
        let run = store.get_run("run-1").expect("get").expect("run");
        let prompt = attempt_prompt(&run, &run.steps[1]).expect("prompt");

        assert!(prompt.contains("## Handoff"));
        assert!(prompt.contains("You are receiving control from: research"));
    }

    struct FakeExecutor;

    #[async_trait::async_trait]
    impl Executor for FakeExecutor {
        async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
            Ok(format!("{} complete", attempt.step_id))
        }
    }

    #[tokio::test]
    async fn controller_runs_sequential_steps_to_completion() {
        let store: Arc<dyn WorkflowStore> = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog {
                definitions: vec![definition],
            },
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity {
                profile: "default".into(),
                provider: "openai-compatible".into(),
                model: "model".into(),
            },
            2,
        );

        let run = controller.start("flow", "request").await.expect("start");
        let completed = controller.wait(&run.id).await.expect("wait");
        assert_eq!(completed.status, RunStatus::Succeeded);
        assert_eq!(completed.steps[0].result, "first complete");
        assert_eq!(completed.steps[1].result, "second complete");
    }

    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_finished_run_writes_one_event_per_step_and_one_for_the_run() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new_with_deadlines(
            Arc::new(Store::open_in_memory()),
            Catalog {
                definitions: vec![definition],
            },
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
            DeadlinePolicy::default(),
            Arc::new(Logger::new(Box::new(CapturedLog(Arc::clone(&bytes))))),
        );
        let run = controller
            .start("flow", "secret input")
            .await
            .expect("start");
        controller.wait(&run.id).await.expect("wait");
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let steps: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("msg=workflow_step_finished"))
            .collect();
        assert_eq!(steps.len(), 2, "{text}");
        for (line, id) in steps.iter().zip(["first", "second"]) {
            for field in [
                "level=INFO".to_string(),
                format!("run_id={}", run.id),
                "workflow=flow".to_string(),
                format!("step_id={id}"),
                "status=succeeded".to_string(),
                "duration_ms=".to_string(),
            ] {
                assert!(line.contains(&field), "{field} missing in {line}");
            }
        }
        let finished: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("msg=workflow_run_finished"))
            .collect();
        assert_eq!(finished.len(), 1, "{text}");
        for field in [format!("run_id={}", run.id), "status=succeeded".to_string()] {
            assert!(
                finished[0].contains(&field),
                "{field} missing in {}",
                finished[0]
            );
        }
        assert!(!text.contains("secret input"), "{text}");
    }

    #[tokio::test]
    async fn controller_runs_handoff_steps_to_completion() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "research"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "review"
needs = ["research"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog::from_definitions(vec![definition]),
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
        );

        let run = controller.start("flow", "").await.expect("start");
        let completed = controller.wait(&run.id).await.expect("wait");
        assert_eq!(completed.status, RunStatus::Succeeded);
        assert_eq!(completed.steps[1].kind, StepKind::Handoff);
        assert_eq!(completed.steps[1].result, "review complete");
    }

    #[tokio::test]
    async fn controller_forks_and_runs_from_the_boundary() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        store
            .create_run("run-1", &definition, "/workspace", "", "", "", "")
            .expect("create");
        let attempt = store
            .claim_ready("run-1", "first", "/tmp/first.jsonl")
            .expect("claim");
        store
            .finish_attempt("run-1", "first", attempt, Ok("first done"))
            .expect("finish");
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog::from_definitions(vec![definition]),
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            1,
        );

        let fork = controller.fork("run-1", "first").await.expect("fork");
        let done = controller.wait(&fork.id).await.expect("wait");

        assert_eq!(done.status, RunStatus::Succeeded);
        assert_eq!(done.forked_from_run_id.as_deref(), Some("run-1"));
        assert_eq!(done.steps[0].result, "first done");
        assert_eq!(done.steps[1].result, "second complete");
    }

    #[tokio::test]
    async fn controller_waits_for_parallel_approval_then_resumes_dependents() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship?"
[[steps]]
id = "deliver"
agent = "worker"
prompt = "deliver"
needs = ["work", "approve"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog::from_definitions(vec![definition]),
            Arc::new(FakeExecutor),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
        );

        let run = controller.start("flow", "").await.expect("start");
        let waiting = controller.wait(&run.id).await.expect("wait");
        assert_eq!(waiting.status, RunStatus::Waiting);
        let request = controller
            .requests(&run.id)
            .expect("requests")
            .pop()
            .expect("request");
        let resumed = controller.respond(&request.id, true).expect("approve");
        assert_eq!(resumed.status, RunStatus::Running);
        let completed = controller.wait(&run.id).await.expect("complete");
        assert_eq!(completed.status, RunStatus::Succeeded);
        assert_eq!(completed.steps[2].result, "deliver complete");
    }

    struct FailAndCancelExecutor {
        sibling_stopped: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Executor for FailAndCancelExecutor {
        async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
            if attempt.step_id == "fail" {
                return Err("failed".to_string());
            }
            control.cancellation_token().cancelled().await;
            self.sibling_stopped.store(true, Ordering::SeqCst);
            Err("context canceled".to_string())
        }
    }

    #[tokio::test]
    async fn failure_waits_for_running_siblings_and_never_schedules_dependents() {
        let stopped = Arc::new(AtomicBool::new(false));
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "fail"
agent = "worker"
prompt = "fail"
[[steps]]
id = "sibling"
agent = "worker"
prompt = "wait"
[[steps]]
id = "downstream"
agent = "worker"
prompt = "never"
needs = ["sibling"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(FailAndCancelExecutor {
                sibling_stopped: Arc::clone(&stopped),
            }),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
        );

        let run = controller.start("flow", "").await.expect("start");
        let failed = controller.wait(&run.id).await.expect("wait");
        assert!(stopped.load(Ordering::SeqCst));
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.steps[2].status, StepStatus::Canceled);
    }

    struct CountingExecutor(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Executor for CountingExecutor {
        async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(format!("{} done", attempt.step_id))
        }
    }

    #[tokio::test]
    async fn resume_schedules_a_committed_ready_step_once() {
        let store = Arc::new(Store::open_in_memory());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        store
            .create_run("run-1", &definition, "/workspace", "", "", "", "")
            .expect("create");
        let attempt = store
            .claim_ready("run-1", "first", "/tmp/first")
            .expect("claim");
        store
            .finish_attempt("run-1", "first", attempt, Ok("first done"))
            .expect("finish");
        let calls = Arc::new(AtomicUsize::new(0));
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            store,
            Catalog::from_definitions(vec![definition]),
            Arc::new(CountingExecutor(Arc::clone(&calls))),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            1,
        );

        controller.resume("run-1", None).await.expect("resume");
        let done = controller.wait("run-1").await.expect("wait");
        assert_eq!(done.status, RunStatus::Succeeded);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct BoundaryExecutor(Arc<Notify>);

    #[async_trait::async_trait]
    impl Executor for BoundaryExecutor {
        async fn execute(&self, _attempt: Attempt, _control: &Control) -> Result<String, String> {
            self.0.notify_one();
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok("boundary".into())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn executor_completion_wins_at_exact_deadline_boundary() {
        let started = Arc::new(Notify::new());
        let definition = definition(
            br#"version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new_with_deadlines(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(BoundaryExecutor(Arc::clone(&started))),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            1,
            DeadlinePolicy {
                step_timeout: Some(Duration::from_secs(1)),
                cancellation_grace: Duration::from_secs(1),
            },
            Arc::new(Logger::discard()),
        );
        let run = controller.start("flow", "").await.expect("start");
        started.notified().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let done = controller.wait(&run.id).await.expect("wait");
        assert_eq!(done.status, RunStatus::Succeeded);
        assert_eq!(done.steps[0].result, "boundary");
    }

    struct TimeoutBatchExecutor {
        sibling_started: Arc<Notify>,
        sibling_release: CancellationToken,
    }

    #[async_trait::async_trait]
    impl Executor for TimeoutBatchExecutor {
        async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
            if attempt.step_id == "timeout" {
                control.cancellation_token().cancelled().await;
                return Err("stopped".into());
            }
            self.sibling_started.notify_one();
            self.sibling_release.cancelled().await;
            Ok("settled".into())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_is_not_retryable_before_parallel_siblings_settle() {
        let sibling_started = Arc::new(Notify::new());
        let sibling_release = CancellationToken::new();
        let definition = definition(
            br#"version = 1
[[steps]]
id = "timeout"
agent = "worker"
prompt = "timeout"
[[steps]]
id = "sibling"
agent = "worker"
prompt = "sibling"
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new_with_deadlines(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(TimeoutBatchExecutor {
                sibling_started: Arc::clone(&sibling_started),
                sibling_release: sibling_release.clone(),
            }),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
            DeadlinePolicy {
                step_timeout: Some(Duration::from_secs(1)),
                cancellation_grace: Duration::from_millis(100),
            },
            Arc::new(Logger::discard()),
        );
        let run = controller.start("flow", "").await.expect("start");
        sibling_started.notified().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            controller.get(&run.id).expect("run").status,
            RunStatus::Running
        );
        assert!(controller.resume(&run.id, Some("timeout")).await.is_err());
        sibling_release.cancel();
        assert_eq!(
            controller.wait(&run.id).await.expect("wait").status,
            RunStatus::Paused
        );
    }

    struct BlockingExecutor {
        started: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl Executor for BlockingExecutor {
        async fn execute(&self, _attempt: Attempt, control: &Control) -> Result<String, String> {
            self.started.notify_one();
            control.cancellation_token().cancelled().await;
            Err("context canceled".to_string())
        }
    }

    #[tokio::test]
    async fn graceful_close_persists_an_interrupted_attempt() {
        let started = Arc::new(Notify::new());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(BlockingExecutor {
                started: Arc::clone(&started),
            }),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            1,
        );
        let run = controller.start("flow", "").await.expect("start");
        started.notified().await;

        controller.close().await;
        let paused = controller.get(&run.id).expect("run");
        assert_eq!(paused.status, RunStatus::Paused);
        assert_eq!(paused.steps[0].status, StepStatus::Interrupted);
    }

    #[tokio::test]
    async fn active_runs_counts_in_flight_runs_and_drops_to_zero_after_close() {
        let started = Arc::new(Notify::new());
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(BlockingExecutor {
                started: Arc::clone(&started),
            }),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            1,
        );
        assert_eq!(controller.active_runs(), 0);

        controller.start("flow", "").await.expect("start");
        started.notified().await;
        assert_eq!(controller.active_runs(), 1);

        controller.close().await;
        assert_eq!(controller.active_runs(), 0);
    }

    const TWO_STEP: &[u8] = br#"
version = 1
[[steps]]
id = "a"
agent = "worker"
prompt = "a"
[[steps]]
id = "b"
agent = "worker"
prompt = "b"
"#;

    #[test]
    fn crash_after_failure_settles_the_run_and_failed_steps_are_retryable() {
        let store = Store::open_in_memory();
        let definition = definition(TWO_STEP);
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        store
            .claim_ready("run-1", "a", "/tmp/a1.jsonl")
            .expect("claim a");
        store
            .claim_ready("run-1", "b", "/tmp/b1.jsonl")
            .expect("claim b");
        store
            .finish_attempt("run-1", "a", 1, Err("boom"))
            .expect("fail a");

        // Process death here: `b` is still running, so recovery interrupts it and
        // pauses the run instead of leaving it `running` with no owner.
        assert_eq!(store.recover("/w").expect("recover"), 1);
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Paused
        );

        // Retrying the surviving sibling must not hide the failed step: once it
        // succeeds the run reaches its durable failure rather than hanging.
        store.retry("run-1", "b").expect("retry b");
        let attempt = store
            .claim_ready("run-1", "b", "/tmp/b2.jsonl")
            .expect("claim b");
        store
            .finish_attempt("run-1", "b", attempt, Ok("b done"))
            .expect("finish b");
        let run = store.get_run("run-1").expect("get").expect("run");
        assert_eq!(run.status, RunStatus::Failed, "{:?}", run.steps);
        assert_eq!(run.steps[0].status, StepStatus::Failed);

        // A failed step is retryable, and retrying it revives the run.
        store.retry("run-1", "a").expect("retry a");
        let attempt = store
            .claim_ready("run-1", "a", "/tmp/a2.jsonl")
            .expect("claim a");
        store
            .finish_attempt("run-1", "a", attempt, Ok("a done"))
            .expect("finish a");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Succeeded
        );
    }

    #[test]
    fn retry_revives_steps_canceled_by_the_failure() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "a"
agent = "worker"
prompt = "a"
[[steps]]
id = "b"
agent = "worker"
prompt = "b"
[[steps]]
id = "c"
agent = "worker"
prompt = "c"
needs = ["b"]
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        store
            .claim_ready("run-1", "a", "/tmp/a1.jsonl")
            .expect("claim a");
        store
            .claim_ready("run-1", "b", "/tmp/b1.jsonl")
            .expect("claim b");
        store
            .finish_attempt("run-1", "a", 1, Err("boom"))
            .expect("fail a");
        store.cancel_attempt("run-1", "b", 1).expect("cancel b");
        store.settle("run-1").expect("settle");

        let run = store.get_run("run-1").expect("get").expect("run");
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.steps[2].status, StepStatus::Canceled);

        // Retrying the failed step re-admits the work the failure canceled.
        store.retry("run-1", "a").expect("retry a");
        let run = store.get_run("run-1").expect("get").expect("run");
        assert_eq!(run.steps[1].status, StepStatus::Ready, "{:?}", run.steps);
        assert_eq!(run.steps[2].status, StepStatus::Pending, "{:?}", run.steps);

        let attempt = store
            .claim_ready("run-1", "a", "/tmp/a2.jsonl")
            .expect("claim a");
        store
            .finish_attempt("run-1", "a", attempt, Ok("a done"))
            .expect("finish a");
        let attempt = store
            .claim_ready("run-1", "b", "/tmp/b2.jsonl")
            .expect("claim b");
        store
            .finish_attempt("run-1", "b", attempt, Ok("b done"))
            .expect("finish b");
        let attempt = store
            .claim_ready("run-1", "c", "/tmp/c1.jsonl")
            .expect("claim c");
        store
            .finish_attempt("run-1", "c", attempt, Ok("c done"))
            .expect("finish c");
        assert_eq!(
            store.get_run("run-1").expect("get").expect("run").status,
            RunStatus::Succeeded
        );
    }

    #[test]
    fn oversized_dependency_results_are_truncated_not_rejected() {
        let store = Store::open_in_memory();
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
        );
        store
            .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        let attempt = store
            .claim_ready("run-1", "first", "/tmp/first.jsonl")
            .expect("claim");
        let huge = "x".repeat(MAX_TEXT_BYTES + 4096);
        store
            .finish_attempt("run-1", "first", attempt, Ok(&huge))
            .expect("finish");

        let run = store.get_run("run-1").expect("get").expect("run");
        assert!(
            run.steps[0].result.len() <= MAX_TEXT_BYTES + 64,
            "stored result was not bounded: {}",
            run.steps[0].result.len()
        );
        let prompt = attempt_prompt(&run, &run.steps[1]).expect("prompt");
        assert!(
            prompt.len() <= MAX_TEXT_BYTES,
            "prompt size {}",
            prompt.len()
        );
        assert!(prompt.contains("[truncated"), "prompt was not marked");
    }

    #[test]
    fn list_runs_pages_newest_first_with_a_cursor() {
        let store = Store::open_in_memory();
        let definition = definition(TWO_STEP);
        for id in ["run-1", "run-2", "run-3"] {
            store
                .create_run(id, &definition, "/w", "p", "provider", "m", "")
                .expect("create");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let page = store
            .list_runs("/w", &ListQuery::page(2, None))
            .expect("page");
        let ids: Vec<&str> = page.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["run-3", "run-2"]);

        let next = store
            .list_runs("/w", &ListQuery::page(2, Some(page[1].created_at.clone())))
            .expect("next page");
        let ids: Vec<&str> = next.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["run-1"]);
        assert!(next[0].steps.len() == 2, "a page keeps each run's steps");
        assert!(
            store
                .list_runs("/other", &ListQuery::all())
                .expect("other workspace")
                .is_empty()
        );
    }

    struct GateExecutor {
        started: Arc<AtomicBool>,
        canceled: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Executor for GateExecutor {
        async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
            if attempt.step_id == "work" {
                self.started.store(true, Ordering::SeqCst);
                control.cancellation_token().cancelled().await;
                self.canceled.store(true, Ordering::SeqCst);
                return Err("context canceled".to_string());
            }
            Ok(format!("{} complete", attempt.step_id))
        }
    }

    #[tokio::test]
    async fn rejecting_a_gate_cancels_a_running_sibling() {
        let started = Arc::new(AtomicBool::new(false));
        let canceled = Arc::new(AtomicBool::new(false));
        let definition = definition(
            br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship?"
[[steps]]
id = "deliver"
agent = "worker"
prompt = "deliver"
needs = ["work", "approve"]
"#,
        );
        let transcripts = tempfile::tempdir().expect("transcripts");
        let controller = Controller::new(
            Arc::new(Store::open_in_memory()),
            Catalog::from_definitions(vec![definition]),
            Arc::new(GateExecutor {
                started: Arc::clone(&started),
                canceled: Arc::clone(&canceled),
            }),
            "/workspace".into(),
            transcripts.path().into(),
            RuntimeIdentity::default(),
            2,
        );

        let run = controller.start("flow", "").await.expect("start");
        for _ in 0..200 {
            if started.load(Ordering::SeqCst)
                && controller.requests(&run.id).expect("requests").len() == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            started.load(Ordering::SeqCst),
            "the sibling attempt never started"
        );
        let request = controller
            .requests(&run.id)
            .expect("requests")
            .pop()
            .expect("request");

        let rejected = controller.respond(&request.id, false).expect("reject");
        assert_eq!(rejected.status, RunStatus::Canceled);
        for _ in 0..200 {
            if canceled.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            canceled.load(Ordering::SeqCst),
            "a rejected run kept executing its sibling"
        );
    }
}
