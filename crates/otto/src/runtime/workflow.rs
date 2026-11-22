//! Durable workflow composition using the existing child-agent runtime.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use nix::fcntl::{Flock, FlockArg};
use otto_core::config::resolve::Runtime;
use otto_core::session::{CURRENT_VERSION, Header, Session};

use crate::runtime::builder::{Builder, MemoryHandle, Runner, SharedSession};
use crate::subagent::runner::{Runner as SubagentRunner, StartRequest};
use crate::subagent::tasks::TaskStatus;
use crate::workflow::{Attempt, Catalog, Controller, DeadlinePolicy, Executor, RuntimeIdentity};
use otto_platform::deadline::Control;
use otto_platform::random::random_id;

struct AgentExecutor {
    owner: Arc<Runner>,
    runner: Arc<SubagentRunner>,
    workspace: String,
    runtime: RuntimeIdentity,
}

#[async_trait::async_trait]
impl Executor for AgentExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        let store = Arc::new(
            otto_session::Store::create(
                &attempt.transcript_root,
                Header {
                    version: CURRENT_VERSION,
                    id: attempt.session_id,
                    workspace: self.workspace.clone(),
                    profile: self.runtime.profile.clone(),
                    provider: self.runtime.provider.clone(),
                    model: self.runtime.model.clone(),
                    created_at: Utc::now(),
                },
            )
            .map_err(|error| error.to_string())?,
        );
        if store.path() != attempt.transcript_path {
            let _ = store.close();
            return Err("workflow transcript path mismatch".to_string());
        }
        let transcript: Arc<dyn Session + Send + Sync> = store.clone();
        let definition = crate::subagent::Definition {
            name: attempt.agent_definition.name,
            description: attempt.agent_definition.description,
            tools: attempt.agent_definition.tools,
            model: attempt.agent_definition.model,
            context: attempt.agent_definition.context,
            write_policy: attempt.agent_definition.write_policy.as_subagent(),
            write_paths: attempt.agent_definition.write_paths,
            body: attempt.agent_definition.body,
            directory: PathBuf::new(),
            path: PathBuf::new(),
            is_skill_derived: false,
        };
        let task = self
            .runner
            .run_with_definition_control(
                StartRequest {
                    prompt: attempt.prompt,
                    description: attempt.step_id,
                    agent: attempt.agent,
                    context: "fresh".to_string(),
                    ..StartRequest::default()
                },
                transcript,
                definition,
                control,
                attempt.budget,
            )
            .await;
        let close = store.close().map_err(|error| error.to_string());
        let task = task?;
        close?;
        match task.status {
            TaskStatus::Succeeded => Ok(task.result),
            TaskStatus::Canceled => Err("context canceled".to_string()),
            TaskStatus::Failed | TaskStatus::Unavailable => Err(task.error),
            TaskStatus::Interrupted => Err(if task.error.is_empty() {
                "workflow child was interrupted".to_string()
            } else {
                task.error
            }),
            TaskStatus::Queued | TaskStatus::Running => {
                Err("workflow child did not reach a terminal state".to_string())
            }
        }
    }

    async fn close(&self) {
        self.owner.close();
        self.owner.session().close_resources().await;
    }
}

pub async fn build_controller(
    builder: Arc<Builder>,
    runtime: &Runtime,
    stderr: &mut (dyn Write + Send),
) -> Result<Arc<Controller>, String> {
    shared_controller(&builder, runtime, stderr).await
}

/// Join active workflow boundaries before an owning frontend exits.
pub async fn close(builder: &Builder) {
    if let Some(controller) = builder.workflows.get() {
        controller.close().await;
    }
}

pub(crate) async fn shared_controller(
    builder: &Builder,
    runtime: &Runtime,
    stderr: &mut (dyn Write + Send),
) -> Result<Arc<Controller>, String> {
    // An Err is not stored: a lock held by another process may be released
    // later, so the next call composes again.
    builder
        .workflows
        .get_or_try_init(|| Box::pin(compose_controller(builder, runtime, stderr)))
        .await
        .cloned()
}

async fn compose_controller(
    builder: &Builder,
    runtime: &Runtime,
    stderr: &mut (dyn Write + Send),
) -> Result<Arc<Controller>, String> {
    let lock = lock_workspace(&builder.home, &builder.workspace_path)?;
    let id = random_id().map_err(|error| format!("create workflow runtime id: {error}"))?;
    let session = SharedSession::new(Arc::new(MemoryHandle::new(Header {
        version: CURRENT_VERSION,
        id,
        workspace: builder.workspace_path.clone(),
        profile: runtime.model_selector.to_string(),
        provider: runtime.provider_type.clone(),
        model: runtime.model.clone(),
        created_at: Utc::now(),
    })));
    let owner = Arc::new(
        builder
            .build_workflow_runner(&session, runtime, stderr)
            .await?,
    );
    let runner = owner
        .subagents()
        .ok_or_else(|| "workflow runtime requires enabled agents".to_string())?;

    let roots = workflow_roots(&builder.home, &builder.workspace_path);
    let (catalog, warnings) = Catalog::discover(&roots, owner.agents());
    for warning in warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    let store = builder
        .storage
        .workflow(&Path::new(&builder.home).join(".otto/workflows.db"))?;
    store
        .recover(&builder.workspace_path)
        .map_err(|error| error.to_string())?;
    let agents = otto_core::config::resolve_agents(
        &builder.config,
        &builder.environment,
        &builder.workspace_path,
    )
    .map_err(|error| error.to_string())?;
    let controller = Controller::new_with_deadlines(
        store,
        catalog,
        Arc::new(AgentExecutor {
            owner,
            runner,
            workspace: builder.workspace_path.clone(),
            runtime: RuntimeIdentity {
                profile: runtime.model_selector.to_string(),
                provider: runtime.provider_name.clone(),
                model: runtime.model.clone(),
            },
        }),
        builder.workspace_path.clone(),
        Path::new(&builder.home).join(".otto/workflow-sessions"),
        RuntimeIdentity {
            profile: runtime.model_selector.to_string(),
            provider: runtime.provider_name.clone(),
            model: runtime.model.clone(),
        },
        usize::try_from(agents.max_parallel).unwrap_or(1),
        DeadlinePolicy {
            step_timeout: runtime.resilience.deadlines.workflow_step_timeout,
            cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
        },
        Arc::clone(&builder.logger),
        runtime.budget,
    );
    controller.attach_approvals(&builder.approvals)?;
    controller.set_guard(Box::new(lock));
    Ok(controller)
}

fn workflow_roots(home: &str, workspace: &str) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(2);
    if !home.is_empty() {
        roots.push(Path::new(home).join(".otto/workflows"));
    }
    roots.push(Path::new(workspace).join(".otto/workflows"));
    roots
}

pub fn lock_workspace(home: &str, workspace: &str) -> Result<Flock<std::fs::File>, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let directory = Path::new(home).join(".otto/workflow-locks");
    std::fs::create_dir_all(&directory).map_err(|_| "workflow runtime unavailable".to_string())?;
    std::fs::set_permissions(
        &directory,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .map_err(|_| "workflow runtime unavailable".to_string())?;
    let key = otto_session::workspace_key(Path::new(workspace))
        .map_err(|_| "workflow runtime unavailable".to_string())?;
    let path = directory.join(format!("{key}.lock"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|_| "workflow runtime unavailable".to_string())?;
    std::fs::set_permissions(
        path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .map_err(|_| "workflow runtime unavailable".to_string())?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock)
        .map_err(|_| "workflow runtime is already active for this workspace".to_string())
}
