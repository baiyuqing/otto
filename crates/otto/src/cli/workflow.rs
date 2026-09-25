//! Durable workflow composition using the existing child-agent runtime.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use nix::fcntl::{Flock, FlockArg};
use otto_core::config::resolve::Runtime;
use otto_core::session::{CURRENT_VERSION, Header, Session};
use tokio_util::sync::CancellationToken;

use super::runtime_builder::{Builder, MemoryHandle, Runner, SharedSession, random_id};
use crate::subagent::runner::{Runner as SubagentRunner, StartRequest};
use crate::subagent::tasks::TaskStatus;
use crate::workflow::{
    ApprovalRequest, Attempt, Catalog, Controller, Executor, Run, RunStatus, RuntimeIdentity, Store,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Run {
        name: String,
        input: String,
    },
    Status {
        run_id: String,
    },
    Resume {
        run_id: String,
        retry: Option<String>,
    },
    Fork {
        run_id: String,
        after_step: String,
    },
    Approve {
        request_id: String,
    },
    Reject {
        request_id: String,
    },
    Cancel {
        run_id: String,
    },
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some(action) = args.first().map(String::as_str) else {
        return Err(
            "otto: workflow requires run, status, resume, fork, approve, reject, or cancel".into(),
        );
    };
    match action {
        "run" => {
            let name = args
                .get(1)
                .filter(|value| !value.starts_with('-'))
                .cloned()
                .ok_or_else(|| "otto: workflow run requires a name".to_string())?;
            let mut input = String::new();
            let mut index = 2;
            while index < args.len() {
                if args[index] != "--input" || index + 1 >= args.len() || !input.is_empty() {
                    return Err("otto: invalid workflow run arguments".to_string());
                }
                input = args[index + 1].clone();
                index += 2;
            }
            Ok(Command::Run { name, input })
        }
        "status" | "approve" | "reject" | "cancel" => {
            if args.len() != 2 {
                return Err(format!("otto: workflow {action} requires one id"));
            }
            Ok(match action {
                "status" => Command::Status {
                    run_id: args[1].clone(),
                },
                "approve" => Command::Approve {
                    request_id: args[1].clone(),
                },
                "reject" => Command::Reject {
                    request_id: args[1].clone(),
                },
                "cancel" => Command::Cancel {
                    run_id: args[1].clone(),
                },
                _ => unreachable!(),
            })
        }
        "resume" => {
            let run_id = args
                .get(1)
                .cloned()
                .ok_or_else(|| "otto: workflow resume requires a run id".to_string())?;
            let retry = match &args[2..] {
                [] => None,
                [flag, step] if flag == "--retry" => Some(step.clone()),
                _ => return Err("otto: invalid workflow resume arguments".to_string()),
            };
            Ok(Command::Resume { run_id, retry })
        }
        "fork" => {
            let run_id = args
                .get(1)
                .cloned()
                .ok_or_else(|| "otto: workflow fork requires a run id".to_string())?;
            let after_step = match &args[2..] {
                [flag, step] if flag == "--after-step" => step.clone(),
                _ => return Err("otto: invalid workflow fork arguments".to_string()),
            };
            Ok(Command::Fork { run_id, after_step })
        }
        _ => Err(format!("otto: unknown workflow command {action:?}")),
    }
}

#[derive(serde::Serialize)]
struct View {
    run: Run,
    requests: Vec<ApprovalRequest>,
}

pub async fn run_command(
    command: Command,
    controller: &Arc<Controller>,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    let mut run = match command {
        Command::Run { name, input } => {
            let run = controller.start(&name, &input).await?;
            controller.wait(&run.id).await?
        }
        Command::Status { run_id } => controller.get(&run_id)?,
        Command::Resume { run_id, retry } => {
            let run = controller.resume(&run_id, retry.as_deref()).await?;
            if run.status == RunStatus::Running {
                controller.wait(&run.id).await?
            } else {
                run
            }
        }
        Command::Fork { run_id, after_step } => {
            let run = controller.fork(&run_id, &after_step).await?;
            if run.status == RunStatus::Running {
                controller.wait(&run.id).await?
            } else {
                run
            }
        }
        Command::Approve { request_id } => {
            let run = controller.respond(&request_id, true)?;
            if run.status == RunStatus::Running {
                controller.wait(&run.id).await?
            } else {
                run
            }
        }
        Command::Reject { request_id } => controller.respond(&request_id, false)?,
        Command::Cancel { run_id } => controller.cancel(&run_id).await?,
    };
    // The driver may have completed between the command's last observation and
    // serialization; read once more for a coherent view.
    run = controller.get(&run.id)?;
    let view = View {
        requests: controller.requests(&run.id)?,
        run,
    };
    serde_json::to_writer_pretty(&mut *stdout, &view)
        .map_err(|_| "write workflow output failed")?;
    writeln!(stdout).map_err(|_| "write workflow output failed".to_string())
}

struct AgentExecutor {
    owner: Arc<Runner>,
    runner: Arc<SubagentRunner>,
    workspace: String,
    runtime: RuntimeIdentity,
}

#[async_trait::async_trait]
impl Executor for AgentExecutor {
    async fn execute(
        &self,
        attempt: Attempt,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let store = Arc::new(
            crate::session::Store::create(
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
            .run_with_definition(
                StartRequest {
                    prompt: attempt.prompt,
                    description: attempt.step_id,
                    agent: attempt.agent,
                    context: "fresh".to_string(),
                    ..StartRequest::default()
                },
                transcript,
                definition,
                cancel,
            )
            .await;
        let close = store.close().map_err(|error| error.to_string());
        let task = task?;
        close?;
        match task.status {
            TaskStatus::Succeeded => Ok(task.result),
            TaskStatus::Canceled => Err("context canceled".to_string()),
            TaskStatus::Failed => Err(task.error),
            TaskStatus::Queued | TaskStatus::Running => {
                Err("workflow child did not reach a terminal state".to_string())
            }
        }
    }

    async fn close(&self) {
        self.owner.close_mcp().await;
        self.owner.close();
    }
}

pub async fn build_controller(
    builder: Arc<Builder>,
    runtime: &Runtime,
    stderr: &mut (dyn Write + Send),
) -> Result<Arc<Controller>, String> {
    let lock = lock_workspace(&builder.home, &builder.workspace_path)?;
    let id = random_id().map_err(|error| format!("create workflow runtime id: {error}"))?;
    let session = SharedSession::new(Arc::new(MemoryHandle::new(Header {
        version: CURRENT_VERSION,
        id,
        workspace: builder.workspace_path.clone(),
        profile: runtime.profile.clone(),
        provider: runtime.provider.clone(),
        model: runtime.model.clone(),
        created_at: Utc::now(),
    })));
    let owner = Arc::new(builder.build_runner(&session, runtime).await?);
    let runner = owner
        .subagents()
        .ok_or_else(|| "workflow runtime requires enabled agents".to_string())?;

    let roots = workflow_roots(&builder.home, &builder.workspace_path);
    let (catalog, warnings) = Catalog::discover(&roots, owner.agents());
    for warning in warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    let store = Arc::new(Store::open(
        &Path::new(&builder.home).join(".otto/workflows.db"),
    )?);
    store.recover(&builder.workspace_path)?;
    let agents = otto_core::config::resolve_agents(
        &builder.config,
        &builder.environment,
        &builder.workspace_path,
    )
    .map_err(|error| error.to_string())?;
    let controller = Controller::new(
        store,
        catalog,
        Arc::new(AgentExecutor {
            owner,
            runner,
            workspace: builder.workspace_path.clone(),
            runtime: RuntimeIdentity {
                profile: runtime.profile.clone(),
                provider: runtime.provider.clone(),
                model: runtime.model.clone(),
            },
        }),
        builder.workspace_path.clone(),
        Path::new(&builder.home).join(".otto/workflow-sessions"),
        RuntimeIdentity {
            profile: runtime.profile.clone(),
            provider: runtime.provider.clone(),
            model: runtime.model.clone(),
        },
        usize::try_from(agents.max_parallel).unwrap_or(1),
    );
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

fn lock_workspace(home: &str, workspace: &str) -> Result<Flock<std::fs::File>, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let directory = Path::new(home).join(".otto/workflow-locks");
    std::fs::create_dir_all(&directory).map_err(|_| "workflow runtime unavailable".to_string())?;
    std::fs::set_permissions(
        &directory,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .map_err(|_| "workflow runtime unavailable".to_string())?;
    let key = crate::session::workspace_key(Path::new(workspace))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_small_workflow_command_surface() {
        assert_eq!(
            parse(&[
                "run".into(),
                "review".into(),
                "--input".into(),
                "change".into()
            ]),
            Ok(Command::Run {
                name: "review".into(),
                input: "change".into(),
            })
        );
        assert_eq!(
            parse(&[
                "resume".into(),
                "run-1".into(),
                "--retry".into(),
                "step".into()
            ]),
            Ok(Command::Resume {
                run_id: "run-1".into(),
                retry: Some("step".into()),
            })
        );
        assert_eq!(
            parse(&[
                "fork".into(),
                "run-1".into(),
                "--after-step".into(),
                "review".into(),
            ]),
            Ok(Command::Fork {
                run_id: "run-1".into(),
                after_step: "review".into(),
            })
        );
        assert!(parse(&["run".into()]).is_err());
    }

    #[test]
    fn workspace_lock_rejects_only_the_same_workspace() {
        let home = tempfile::tempdir().expect("home");
        let first_workspace = tempfile::tempdir().expect("workspace");
        let second_workspace = tempfile::tempdir().expect("other workspace");
        let _first = lock_workspace(
            &home.path().to_string_lossy(),
            &first_workspace.path().to_string_lossy(),
        )
        .expect("first lock");
        assert_eq!(
            lock_workspace(
                &home.path().to_string_lossy(),
                &first_workspace.path().to_string_lossy(),
            )
            .expect_err("same workspace"),
            "workflow runtime is already active for this workspace"
        );
        lock_workspace(
            &home.path().to_string_lossy(),
            &second_workspace.path().to_string_lossy(),
        )
        .expect("other workspace");
    }
}
