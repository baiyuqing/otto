use super::*;
use crate::fixtures::subagent_doubles::{
    FakeProvider, RouteStep, StubTool, assistant_text, assistant_tool_call, match_any, raw, stub,
    test_config, wait_status,
};
use crate::subagent::Definition;
use crate::subagent::runner::{Config, StartRequest};
use otto_core::agent::inbox::NotificationKind;
use otto_core::model::Usage;
use otto_core::tool::ToolResult;
use std::sync::Arc;

fn runner(config: Config) -> Arc<Runner> {
    let (runner, _) = Runner::new(config).expect("the test config is valid");
    Arc::new(runner)
}

fn tool_named(runner: &Arc<Runner>, name: &str) -> Box<dyn Tool + Send + Sync> {
    tools(runner)
        .into_iter()
        .find(|tool| tool.definition().name == name)
        .unwrap_or_else(|| panic!("tool {name:?} is registered"))
}

async fn run(tool: &dyn Tool, arguments: &str) -> ToolResult {
    tool.execute(&raw(arguments), &CancellationToken::new())
        .await
}

async fn wait_final(tasks: &Tasks, id: &str) -> Task {
    let cancel = CancellationToken::new();
    tokio::select! {
        result = tasks.wait(id, &cancel) => result.expect("the task exists"),
        () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            panic!("task {id} did not finish in time")
        }
    }
}

/// A hook that blocks every provider call until `release` fires, or the
/// child is cancelled.
fn block_until(release: CancellationToken) -> crate::fixtures::subagent_doubles::Hook {
    Arc::new(move |cancel, _request| {
        let release = release.clone();
        Box::pin(async move {
            tokio::select! {
                () = release.cancelled() => {}
                () = cancel.cancelled() => {}
            }
        })
    })
}

fn write_skill_with_vague_output(directory: &std::path::Path) {
    std::fs::create_dir_all(directory).expect("mkdir");
    std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: reviewer\ndescription: reviews things\ninput: a path\noutput: the result\n---\nBody.\n",
        )
        .expect("write SKILL.md");
}

fn checked_skill(directory: &std::path::Path) -> crate::skill::Skill {
    crate::skill::Skill {
        name: "reviewer".to_string(),
        description: String::new(),
        contract: None,
        directory: directory.to_path_buf(),
        path: directory.join("SKILL.md"),
    }
}

mod messaging;
mod reports;
mod start;
mod status;
