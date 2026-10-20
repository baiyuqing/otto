//! Durable workflow composition using the existing child-agent runtime.

#[cfg(test)]
use crate::runtime::workflow::lock_workspace;
use crate::workflow::{ApprovalRequest, Controller, Run, RunStatus};
use std::io::Write;
use std::sync::Arc;

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
    let run = execute(command, controller)
        .await
        .map_err(|error| error.to_string())?;
    // The driver may have completed between the command's last observation and
    // serialization; read once more for a coherent view.
    let run = controller.get(&run.id).map_err(|error| error.to_string())?;
    let view = View {
        requests: controller
            .requests(&run.id)
            .map_err(|error| error.to_string())?,
        run,
    };
    serde_json::to_writer_pretty(&mut *stdout, &view)
        .map_err(|_| "write workflow output failed")?;
    writeln!(stdout).map_err(|_| "write workflow output failed".to_string())
}

async fn execute(
    command: Command,
    controller: &Arc<Controller>,
) -> Result<Run, crate::workflow::Error> {
    Ok(match command {
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
    })
}

pub use crate::runtime::workflow::build_controller;

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
