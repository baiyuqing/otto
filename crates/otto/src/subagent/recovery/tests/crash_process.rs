//! Only a controlled test subprocess is killed. No provider, user Otto process,
//! external service, credentials or uncertain-effect replay is involved.

mod result_boundary;

use super::*;
use crate::fixtures::subagent_doubles::{
    FakeProvider, assistant_text, assistant_tool_call, test_config,
};
use crate::subagent::runner::{Runner, StartRequest};
use crate::tool::Tool;
use otto_core::model::{ToolDefinition, Usage};
use otto_core::tool::ToolResult;
use serde_json::value::RawValue;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const HELPER: &str = "subagent::recovery::tests::crash_process::controlled_crash_writer";

struct EffectTool {
    marker: PathBuf,
    return_result: bool,
}

#[async_trait::async_trait]
impl Tool for EffectTool {
    fn definition(&self) -> ToolDefinition {
        crate::tool::definition(
            "effect",
            "controlled offline effect",
            serde_json::json!({
                "type": "object", "properties": {}, "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, _: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let mut marker = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.marker)
            .unwrap();
        marker.write_all(b"once\n").unwrap();
        marker.sync_all().unwrap();
        if !self.return_result {
            cancel.cancelled().await;
        }
        ToolResult {
            content: "effect recorded once".into(),
            ..ToolResult::default()
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "invoked only as the controlled subprocess of the SIGKILL acceptance test"]
async fn controlled_crash_writer() {
    let root =
        PathBuf::from(std::env::var_os("OTTO_CRASH_FIXTURE_ROOT").expect("explicit fixture root"));
    let mode = std::env::var("OTTO_CRASH_MODE").unwrap();
    let return_result = mode != "executing";
    let before_record = mode == "before_record";
    let parent = Arc::new(parent(&root));
    let parent_path = PathBuf::from(parent.path());
    std::fs::write(root.join("parent-path"), parent.path()).unwrap();
    let tasks = Arc::new(Tasks::new().with_admission_transcript(parent));
    let provider = FakeProvider::new();
    provider.add_route(
        |_| true,
        vec![
            assistant_tool_call("effect-call", "effect", "{}", Usage::default()),
            assistant_text("must never be reached before kill", Usage::default()),
        ],
    );
    let root_for_hook = root.clone();
    provider.set_hook(Arc::new(move |_cancel, request| {
        let idle = request
            .messages
            .iter()
            .any(|message| message.role == Role::User && message.text().starts_with("idle"));
        let result = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool);
        let root = root_for_hook.clone();
        Box::pin(async move {
            if result {
                std::fs::write(root.join("result-durable"), "ready").unwrap();
            }
            if idle || result {
                std::future::pending::<()>().await;
            }
        })
    }));
    let mut config = test_config(
        &provider,
        &tasks,
        vec![Box::new(EffectTool {
            marker: root.join("external-marker"),
            return_result,
        })],
    );
    config.max_parallel = 3;
    let factory_parent = parent_path.clone();
    let boundary = root.join("result-before-record");
    config.child_session = Some(Arc::new(move |task_id, session_id| {
        let name = format!("{task_id}-{session_id}");
        let driver: Arc<dyn crate::fsops::files::FileDriver> = if before_record {
            result_boundary::driver(boundary.clone())
        } else {
            Arc::new(crate::fsops::PosixDriver)
        };
        let store = Store::create_child_lazy_with_driver(
            &factory_parent,
            &name,
            Header {
                id: session_id.into(),
                workspace: factory_parent.parent().unwrap().to_string_lossy().into(),
                provider: "openai-compatible".into(),
                model: "test".into(),
                created_at: chrono::Utc::now(),
                ..Header::default()
            },
            driver,
        )
        .map_err(|error| error.to_string())?;
        Ok(Some((
            Arc::new(store),
            Store::child_path(&factory_parent, &name)
                .to_string_lossy()
                .into(),
        )))
    }));
    let (runner, _) = Runner::new(config).unwrap();
    let runner = Arc::new(runner);
    for prompt in ["effect", "idle-two", "idle-three", "idle-queued"] {
        runner
            .start(StartRequest {
                prompt: prompt.into(),
                ..StartRequest::default()
            })
            .await
            .unwrap();
    }
    // Precisely the admitted-before-child-spec window, without executing it.
    let admitted = tasks.add(Task::default(), None, None).unwrap();
    assert_eq!(admitted.id, "t5");
    tasks.persist_admission(&admitted).await.unwrap();
    // Terminal append happened, but no completion notice was enqueued yet.
    let completed = child(&parent_path, "t6");
    completed
        .append_custom_entry("otto.task_result", r#"{"status":"succeeded","error":""}"#)
        .unwrap();
    drop(completed);
    loop {
        let all = tasks.list();
        let active = all
            .iter()
            .filter(|task| task.status == TaskStatus::Running)
            .count();
        let reached = if before_record {
            root.join("result-before-record").exists()
        } else if return_result {
            root.join("result-durable").exists()
        } else {
            root.join("external-marker").exists()
        };
        if active == 3 && tasks.get("t4").unwrap().status == TaskStatus::Queued && reached {
            break;
        }
        tokio::task::yield_now().await;
    }
    std::fs::write(
        root.join("ready"),
        "three active; queued; admission; terminal",
    )
    .unwrap();
    std::future::pending::<()>().await;
}

struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn sigkill_three_active_children_recover_without_duplicate_external_effects() {
    use std::os::unix::process::ExitStatusExt;
    for mode in ["executing", "before_record", "after_record"] {
        let root = tempfile::tempdir().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER, "--ignored", "--nocapture"])
            .env("OTTO_CRASH_FIXTURE_ROOT", root.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        command.env("OTTO_CRASH_MODE", mode);
        let mut process = KillOnDrop(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(15);
        while !root.path().join("ready").exists() {
            if let Some(status) = process.0.try_wait().unwrap() {
                use std::io::Read;
                let mut stderr = String::new();
                process
                    .0
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("controlled writer exited {status}: {stderr}");
            }
            assert!(
                Instant::now() < deadline,
                "controlled writer never reached explicit crash boundary"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        process.0.kill().unwrap();
        assert_eq!(process.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        let path = PathBuf::from(std::fs::read_to_string(root.path().join("parent-path")).unwrap());
        let inbox = Inbox::default();
        let tasks = Tasks::new();
        activate(&path, &tasks, &inbox, &mut Vec::new());
        assert_eq!(
            tasks.list().len(),
            6,
            "registry hydration must include all durable admissions/children"
        );
        for id in ["t1", "t2", "t3", "t4"] {
            assert_eq!(tasks.get(id).unwrap().status, TaskStatus::Interrupted);
        }
        assert_eq!(tasks.get("t6").unwrap().status, TaskStatus::Succeeded);
        assert_eq!(tasks.get("t5").unwrap().status, TaskStatus::Unavailable);
        assert!(tasks.get("t5").unwrap().session_path.is_empty());
        assert_eq!(
            inbox.len(),
            5,
            "terminal and reconciled results have durable notice keys"
        );
        let records = interrupted::inventory(&path).records;
        assert_eq!(records.len(), 5);
        assert!(
            records.iter().all(|record| record.final_status.is_some()),
            "reconciliation must persist interruption"
        );
        let before: Vec<_> = records
            .iter()
            .map(|record| std::fs::read(&record.path).unwrap())
            .collect();
        let reopened = Tasks::new();
        activate(&path, &reopened, &inbox, &mut Vec::new());
        assert_eq!(tasks.list(), reopened.list());
        assert_eq!(inbox.len(), 5);
        let after: Vec<_> = records
            .iter()
            .map(|record| std::fs::read(&record.path).unwrap())
            .collect();
        assert_eq!(before, after);
        assert_eq!(
            std::fs::read_to_string(root.path().join("external-marker")).unwrap(),
            "once\n"
        );
        assert_eq!(tasks.add(Task::default(), None, None).unwrap().id, "t7");
    }
}
