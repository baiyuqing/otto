//! Serve-owned delivery broker, runtime, service and manager.

use super::*;

/// Serve-owned delivery preflight broker. It retains only weak process
/// references: installing it into a parent-tool slot cannot retain a cycle
/// through loaded hosts or builders. It returns immutable previews only, never
/// a target workspace handle or generic command executor.
///
/// The workspace registry is attached after construction. The parent-tool slot
/// must be installed before the workflow controller is built, while
/// `Workspaces` owns that controller through its host, so the weak registry
/// reference cannot be a constructor argument.
#[derive(Clone)]
pub(super) struct DeliveryBroker {
    pub(super) workspaces: Arc<std::sync::RwLock<Option<Weak<Workspaces>>>>,
    pub(super) shared: Weak<crate::runtime::builder::Shared>,
    pub(super) runtime: Runtime,
    pub(super) cancel: CancellationToken,
}

impl DeliveryBroker {
    pub(super) fn new(
        shared: &Arc<crate::runtime::builder::Shared>,
        runtime: Runtime,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            workspaces: Arc::new(std::sync::RwLock::new(None)),
            shared: Arc::downgrade(shared),
            runtime,
            cancel,
        }
    }

    /// Publishes the workspace registry once it exists. This happens before
    /// the server accepts a request, so no delivery can observe the gap.
    pub(super) fn attach_workspaces(&self, workspaces: &Arc<Workspaces>) {
        *self
            .workspaces
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Arc::downgrade(workspaces));
    }

    pub(super) fn workspaces(&self) -> Result<Arc<Workspaces>, String> {
        self.workspaces
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| "delivery broker is no longer available".to_string())
    }

    async fn target_host(
        &self,
        requested: &str,
        cancel: &CancellationToken,
    ) -> Result<
        (
            PathBuf,
            crate::delivery::AdmissionPolicy,
            Arc<WorkspaceHost>,
        ),
        String,
    > {
        if cancel.is_cancelled() || self.cancel.is_cancelled() {
            return Err("delivery target preflight cancelled".into());
        }
        let workspaces = self.workspaces()?;
        let shared = self
            .shared
            .upgrade()
            .ok_or("delivery broker is no longer available")?;
        let admitted = workspaces
            .admit(requested)
            .map_err(|_| "delivery target is not an admitted workspace")?;
        let policy = crate::delivery::AdmissionPolicy {
            startup: PathBuf::from(&workspaces.startup),
            roots: workspaces.roots.clone(),
            trusted: trusted_projects(&workspaces.config_path),
        };
        let host = match workspaces.host(&admitted.to_string_lossy()).await {
            Some(host) => host,
            None => {
                let mut stderr = std::io::sink();
                workspaces
                    .load(
                        &admitted,
                        &shared,
                        &self.runtime,
                        cancel,
                        &mut stderr,
                        false,
                    )
                    .await?
                    .0
            }
        };
        Ok((admitted, policy, host))
    }
}

#[async_trait]
impl crate::delivery::Preflight for DeliveryBroker {
    async fn preview(
        &self,
        source_workspace: &str,
        request: &crate::delivery::Request,
        cancel: &CancellationToken,
    ) -> Result<crate::delivery::Preview, String> {
        let (admitted, policy, host) = self.target_host(&request.target, cancel).await?;
        let executor = host
            .builder
            .command_executor
            .as_ref()
            .cloned()
            .ok_or("delivery target sandbox is unavailable")?;
        let environment = host.builder.sandbox_environment.clone().unwrap_or_default();
        crate::delivery::Preflight::preview(
            &crate::delivery::SandboxPreflight::new(
                policy,
                crate::delivery::SandboxTargetInspector::new(executor, environment),
            ),
            source_workspace,
            &crate::delivery::Request {
                target: admitted.to_string_lossy().into_owned(),
                objective: request.objective.clone(),
                branch: request.branch.clone(),
                source_refs: request.source_refs.clone(),
            },
            cancel,
        )
        .await
    }
}

/// The terminal target-turn boundary, reached only after the constrained
/// worktree effect and worktree-root builder composition have completed.
/// It cannot run setup Git or change roots.
#[async_trait]
pub(super) trait DeliveryRun: Send {
    fn target_session(&self) -> String;
    fn worktree(&self) -> String;
    async fn run_frozen(&self, prompt: &str, cancel: &CancellationToken) -> Result<String, String>;
    async fn close(self: Box<Self>);
}

/// Server-owned resources for one started delivery. None of these values cross
/// the delivery service boundary into a source session/tool.
struct DeliveryRuntime {
    _primary_host: Arc<WorkspaceHost>,
    worktree_sandbox: crate::runtime::startup::WorkspaceSandbox,
    _worktree_builder: Arc<Builder>,
    worktree_session: crate::runtime::builder::SharedSession,
    worktree_runner: Arc<crate::runtime::builder::Runner>,
}

#[async_trait]
impl DeliveryRun for DeliveryRuntime {
    fn target_session(&self) -> String {
        self.worktree_session.header().id
    }

    fn worktree(&self) -> String {
        self.worktree_session.header().workspace
    }

    async fn run_frozen(&self, prompt: &str, cancel: &CancellationToken) -> Result<String, String> {
        let mut emit = |_| {};
        self.worktree_runner
            .run(prompt, &mut emit, cancel)
            .await
            .map_err(|error| error.to_string())?;
        Ok(self
            .worktree_session
            .messages()
            .into_iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| message.text())
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| "target delivery runner completed".into()))
    }

    async fn close(self: Box<Self>) {
        self.worktree_runner.close_mcp().await;
        self.worktree_runner.close();
        let _ = self.worktree_session.close();
        let _ = self.worktree_sandbox.control.close().await;
    }
}

struct PreparedDelivery {
    primary_host: Arc<WorkspaceHost>,
    pub(super) executor: crate::delivery::WorktreeExecutor,
}

/// Receipt-owned delivery effect service, private to `otto serve`. Its only
/// mutable Git operation is `WorktreeExecutor::add`; every target runner is
/// built after that operation around the newly-created worktree root.
pub(super) struct DeliveryService {
    pub(super) broker: DeliveryBroker,
    pub(super) receipts: Arc<crate::delivery::ReceiptStore>,
    /// Append-only source transcripts keyed by approval id. These are never
    /// passed to the target runner and contain no source filesystem handle.
    pub(super) source_sessions: Mutex<HashMap<String, crate::runtime::builder::SharedSession>>,
    prepared: Mutex<HashMap<String, PreparedDelivery>>,
    pub(super) runtimes: Mutex<HashMap<String, Box<dyn DeliveryRun>>>,
    /// Test-only injection at the post-setup worktree-runtime boundary. It
    /// is absent from production, so it cannot bypass constrained setup.
    #[cfg(test)]
    ready_runs: Mutex<HashMap<String, Box<dyn DeliveryRun>>>,
}

impl DeliveryService {
    pub(super) fn new(
        broker: DeliveryBroker,
        receipts: Arc<crate::delivery::ReceiptStore>,
    ) -> Result<Self, String> {
        // A process crash after `begin` has unknown external Git outcome.
        // Never infer success or replay it on construction.
        receipts.mark_interrupted_unknown()?;
        // A target run still `running` here was interrupted with the process.
        receipts.mark_running_tasks_unknown()?;
        Ok(Self {
            broker,
            receipts,
            source_sessions: Mutex::new(HashMap::new()),
            prepared: Mutex::new(HashMap::new()),
            runtimes: Mutex::new(HashMap::new()),
            #[cfg(test)]
            ready_runs: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn bind_source_session(
        &self,
        approval_id: &str,
        source: crate::runtime::builder::SharedSession,
    ) -> Result<(), String> {
        self.source_sessions
            .lock()
            .map_err(|_| "delivery source session store unavailable")?
            .insert(approval_id.into(), source);
        Ok(())
    }

    async fn prepare_target(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<PreparedDelivery, String> {
        let (target, _policy, host) = self
            .broker
            .target_host(
                &receipt.preview.plan.target.repository.to_string_lossy(),
                cancel,
            )
            .await?;
        if target != receipt.preview.plan.target.repository {
            return Err("delivery target changed before worktree setup".into());
        }
        let executor = host
            .delivery_setup
            .as_ref()
            .ok_or("delivery target does not have constrained worktree setup capability")?
            .worktree_executor();
        Ok(PreparedDelivery {
            primary_host: host,
            executor,
        })
    }

    pub(super) fn source_session(
        &self,
        approval_id: &str,
    ) -> Result<crate::runtime::builder::SharedSession, String> {
        self.source_sessions
            .lock()
            .map_err(|_| "delivery source session store unavailable")?
            .get(approval_id)
            .cloned()
            .ok_or("delivery receipt has no source session binding".to_string())
    }

    pub(super) async fn notify_source(
        &self,
        task: &crate::delivery::StoredTask,
    ) -> Result<(), String> {
        let Some(task) = self.receipts.claim_source_notification(&task.approval_id)? else {
            return Ok(());
        };
        let source = self.source_session(&task.approval_id)?;
        let summary = match task.state {
            crate::delivery::TaskState::Succeeded => format!(
                "Delivery task {} completed. {}",
                task.approval_id, task.result
            ),
            crate::delivery::TaskState::Canceled => {
                format!("Delivery task {} was canceled.", task.approval_id)
            }
            crate::delivery::TaskState::Failed | crate::delivery::TaskState::Unknown => format!(
                "Delivery task {} did not complete: {}",
                task.approval_id, task.error
            ),
            crate::delivery::TaskState::Running => return Ok(()),
        };
        source
            .append(Message {
                role: Role::Assistant,
                blocks: vec![Block::text(summary)],
                created_at: Utc::now(),
                ..Message::default()
            })
            .await
            .map_err(|_| "delivery source notification could not be recorded".into())
    }

    pub(super) async fn finish_run(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let prompt = crate::delivery::render_handoff_prompt(&receipt.preview.handoff);
        let runtime = self
            .runtimes
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .remove(&receipt.id)
            .ok_or("delivery runtime is unavailable")?;
        let outcome = runtime.run_frozen(&prompt, cancel).await;
        let (state, result, error) = match outcome {
            Ok(result) => (crate::delivery::TaskState::Succeeded, result, String::new()),
            Err(error) if cancel.is_cancelled() => {
                (crate::delivery::TaskState::Canceled, String::new(), error)
            }
            Err(error) => (crate::delivery::TaskState::Failed, String::new(), error),
        };
        runtime.close().await;
        let task = self
            .receipts
            .finish_task(&receipt.id, state, &result, &error)?;
        self.notify_source(&task).await
    }

    #[cfg(test)]
    pub(super) fn inject_ready_run(
        &self,
        approval_id: &str,
        run: Box<dyn DeliveryRun>,
    ) -> Result<(), String> {
        self.ready_runs
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .insert(approval_id.into(), run);
        Ok(())
    }

    async fn start_worktree_runner(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<Box<dyn DeliveryRun>, String> {
        #[cfg(test)]
        if let Some(run) = self
            .ready_runs
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .remove(&receipt.id)
        {
            return Ok(run);
        }
        let prepared = self
            .prepared
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .remove(&receipt.id)
            .ok_or("delivery receipt has no prepared target capability")?;
        prepared
            .executor
            .add(&receipt.preview.plan, &receipt.base, cancel)
            .await?;

        let shared = self
            .broker
            .shared
            .upgrade()
            .ok_or("delivery broker is no longer available")?;
        let mut stderr = std::io::sink();
        let (builder, sandbox) = load_workspace(
            shared,
            &receipt.preview.plan.path,
            &Disabled::default(),
            cancel,
            &mut stderr,
        )
        .await?;
        let builder = Arc::new(builder);
        if builder.workspace_path != receipt.preview.plan.path.to_string_lossy() {
            return Err("delivery worktree builder root is invalid".into());
        }
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &builder.config,
            &builder.environment,
            None,
            &builder.overrides,
        )
        .map_err(|error| error.to_string())?;
        let session = builder
            .create_session(&runtime)
            .map_err(|error| error.to_string())?;
        let runner = Arc::new(
            builder
                .build_runner_quiet(&session, &runtime)
                .await
                .map_err(|error| error.to_string())?,
        );
        self.receipts.bind_session(
            &receipt.id,
            &session.header().id,
            Path::new(&builder.workspace_path),
        )?;
        Ok(Box::new(DeliveryRuntime {
            _primary_host: prepared.primary_host,
            worktree_sandbox: sandbox,
            _worktree_builder: builder,
            worktree_session: session,
            worktree_runner: runner,
        }))
    }

    pub(super) async fn shutdown(&self) {
        let runtimes = self
            .runtimes
            .lock()
            .map(|mut runtimes| std::mem::take(&mut *runtimes))
            .unwrap_or_default();
        for (_, runtime) in runtimes {
            runtime.close().await;
        }
    }
}

#[async_trait]
impl crate::delivery::Service for DeliveryService {
    async fn prepare(
        &self,
        approval_id: &str,
        preview: &crate::delivery::Preview,
    ) -> Result<crate::delivery::Receipt, String> {
        let binding = crate::delivery::ReceiptBinding {
            approval_id: approval_id.into(),
            source_workspace: preview.source_workspace.clone(),
            target_repository: preview
                .plan
                .target
                .repository
                .to_string_lossy()
                .into_owned(),
            target_common_dir: preview
                .plan
                .target
                .common_dir
                .to_string_lossy()
                .into_owned(),
            worktree: preview.plan.path.to_string_lossy().into_owned(),
            branch: preview.plan.branch.clone(),
            base: preview.base.clone(),
            handoff_digest: preview.handoff.digest.clone(),
        };
        let stored = self.receipts.prepare(binding)?;
        if stored.state == crate::delivery::ReceiptState::Unknown {
            return Err(
                "delivery receipt outcome is unknown; inspect the worktree before retrying".into(),
            );
        }
        let receipt = crate::delivery::Receipt {
            id: approval_id.into(),
            preview: preview.clone(),
            base: stored.binding.base,
        };
        #[cfg(test)]
        let has_ready_run = self
            .ready_runs
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .contains_key(approval_id);
        #[cfg(not(test))]
        let has_ready_run = false;
        if !has_ready_run {
            let prepared = self
                .prepare_target(&receipt, &CancellationToken::new())
                .await?;
            self.prepared
                .lock()
                .map_err(|_| "delivery runtime store unavailable")?
                .insert(approval_id.into(), prepared);
        }
        Ok(receipt)
    }

    async fn start(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let stored = self.receipts.begin(&receipt.id)?;
        if stored.binding.worktree != receipt.preview.plan.path.to_string_lossy()
            || stored.binding.base != receipt.base
        {
            let _ = self.receipts.unknown(&receipt.id);
            return Err("delivery receipt binding changed before worktree setup".into());
        }
        if self.source_session(&receipt.id).is_err() {
            let _ = self.receipts.unknown(&receipt.id);
            return Err("delivery receipt has no source session binding".into());
        }
        let runtime = match self.start_worktree_runner(receipt, cancel).await {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = self.receipts.unknown(&receipt.id);
                return Err(error);
            }
        };
        let source_session = self.source_session(&receipt.id)?.header().id;
        let target_session = runtime.target_session();
        let worktree = runtime.worktree();
        self.receipts.begin_task(
            &receipt.id,
            &source_session,
            &target_session,
            Path::new(&worktree),
        )?;
        self.runtimes
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .insert(receipt.id.clone(), runtime);
        self.receipts.started(&receipt.id)?;
        self.finish_run(receipt, cancel).await?;
        Ok(format!("delivery task completed: {}", receipt.id))
    }
}

/// The deliberately narrow delivery status projection suitable for a future
/// server/ACP read-only aggregation. Paths and execution capabilities never
/// cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeliveryStatus {
    pub(super) approval_id: String,
    pub(super) state: crate::delivery::TaskState,
    pub(super) result: String,
    pub(super) error: String,
}

/// Serve-owned lifecycle owner for delivery receipts and their private target
/// runtimes. It is not a tool factory and has no model-visible API.
pub(super) struct DeliveryManager {
    pub(super) service: Arc<DeliveryService>,
    pub(super) receipts: Arc<crate::delivery::ReceiptStore>,
    /// Retained for late workspace attachment; the service owns a clone.
    pub(super) broker: DeliveryBroker,
}

impl DeliveryManager {
    pub(super) fn open(
        home: &str,
        shared: &Arc<crate::runtime::builder::Shared>,
        runtime: Runtime,
        cancel: CancellationToken,
    ) -> Result<Self, String> {
        let path = Path::new(home).join(".otto/delivery-receipts.db");
        let receipts = Arc::new(crate::delivery::ReceiptStore::open(&path)?);
        let broker = DeliveryBroker::new(shared, runtime, cancel);
        let service = Arc::new(DeliveryService::new(broker.clone(), Arc::clone(&receipts))?);
        Ok(Self {
            service,
            receipts,
            broker,
        })
    }

    /// Publishes the workspace registry the acceptance broker targets. Called
    /// once the registry exists and before the server accepts a request.
    pub(super) fn attach_workspaces(&self, workspaces: &Arc<Workspaces>) {
        self.broker.attach_workspaces(workspaces);
    }

    pub(super) fn bind_source_session(
        &self,
        approval_id: &str,
        source: crate::runtime::builder::SharedSession,
    ) -> Result<(), String> {
        self.service.bind_source_session(approval_id, source)
    }

    pub(super) fn status(
        &self,
        source_session: &str,
        approval_id: &str,
    ) -> Result<Option<DeliveryStatus>, String> {
        self.receipts.task(approval_id).map(|task| {
            task.filter(|task| task.source_session == source_session)
                .map(|task| DeliveryStatus {
                    approval_id: task.approval_id,
                    state: task.state,
                    result: task.result,
                    error: task.error,
                })
        })
    }

    fn service_for(
        self: &Arc<Self>,
        source: crate::runtime::builder::SharedSession,
    ) -> Arc<dyn crate::delivery::Service> {
        Arc::new(SourceDeliveryService {
            manager: Arc::clone(self),
            source,
        })
    }

    pub(super) async fn shutdown(&self) {
        self.service.shutdown().await;
    }
}

/// Per-parent source binding for a delivery receipt. It has no target host,
/// filesystem root, sandbox environment, or command-execution API.
struct SourceDeliveryService {
    pub(super) manager: Arc<DeliveryManager>,
    pub(super) source: crate::runtime::builder::SharedSession,
}

#[async_trait]
impl crate::delivery::Service for SourceDeliveryService {
    async fn prepare(
        &self,
        approval_id: &str,
        preview: &crate::delivery::Preview,
    ) -> Result<crate::delivery::Receipt, String> {
        self.manager
            .bind_source_session(approval_id, self.source.clone())?;
        self.manager.service.prepare(approval_id, preview).await
    }

    async fn start(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.manager.service.start(receipt, cancel).await
    }
}

/// Parent-only serve tool factory. The returned tool gets only its source
/// identity/transcript and delivery trait ports; target capabilities remain in
/// the manager and broker.
pub(super) struct DeliveryToolFactory {
    pub(super) manager: Arc<DeliveryManager>,
    pub(super) preflight: Arc<dyn crate::delivery::Preflight>,
}

impl DeliveryToolFactory {
    pub(super) fn new(manager: Arc<DeliveryManager>) -> Self {
        // The broker is shared with the manager, so the late
        // `attach_workspaces` call is visible to this preflight.
        let preflight: Arc<dyn crate::delivery::Preflight> = Arc::new(manager.broker.clone());
        Self { manager, preflight }
    }
}

impl crate::runtime::builder::ParentToolFactory for DeliveryToolFactory {
    fn tools(
        &self,
        context: &crate::runtime::builder::ParentToolContext,
    ) -> Result<Vec<Box<dyn crate::tool::Tool + Send + Sync>>, String> {
        let service = self.manager.service_for(context.source_session.clone());
        Ok(vec![Box::new(crate::delivery::StartDeliveryTask::new(
            context.workspace.clone(),
            service,
            Arc::clone(&self.preflight),
        ))])
    }
}
