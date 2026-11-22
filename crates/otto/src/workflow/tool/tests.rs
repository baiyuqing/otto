use super::*;
use crate::fixtures::subagent_doubles::{
    FakeProvider, assistant_text, assistant_tool_call, match_any,
};
use crate::workflow::conversation_tests::{controller, fixture, session};
use crate::workflow::{RunStatus, Store, WorkflowStore};
use otto_core::model::{BlockType, OperationId, Usage};
use otto_core::session::Session;

async fn invoke(tool: &dyn Tool, arguments: &str, operation: &str) -> ToolResult {
    let arguments = RawValue::from_string(arguments.into()).unwrap();
    let operation = OperationId::new(operation).unwrap();
    tool.execute_call(
        ToolCall {
            tool_call_id: "provider-call",
            operation_id: &operation,
            name: &tool.definition().name,
            arguments: &arguments,
            attempt: 1,
        },
        &CancellationToken::new(),
    )
    .await
}
#[tokio::test]
async fn strict_schemas_bounds_trusted_identity_and_no_session_guidance() {
    let (dir, catalog) = fixture();
    let owner = controller(
        Arc::new(Store::open_in_memory()),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let parent = Parent::new(
        Ok(owner.clone()),
        "source".into(),
        false,
        std::sync::Arc::new(otto_core::agent::budget::SessionBudgetCell::new(
            otto_core::agent::budget::untimed(),
        )),
        Default::default(),
    );
    let tools = parent.tools();
    assert_eq!(tools.len(), 4);
    for tool in &tools {
        assert!(
            crate::subagent::runner::EXCLUDED_CHILD_TOOLS
                .contains(&tool.definition().name.as_str())
        );
        assert!(
            invoke(tool.as_ref(), r#"{"workspace":"/escape"}"#, "op")
                .await
                .is_error
        );
        assert!(
            tool.execute(
                &RawValue::from_string("{}".into()).unwrap(),
                &CancellationToken::new()
            )
            .await
            .is_error
        );
    }
    let inspected = invoke(
        tools[1].as_ref(),
        r#"{"name":"documentation-review"}"#,
        "inspect",
    )
    .await;
    assert!(!inspected.is_error);
    assert!(inspected.content.contains("read_only"));
    let long = json!({"name":"documentation-review","input":"é".repeat(32769)});
    assert!(
        invoke(tools[2].as_ref(), &long.to_string(), "too-long")
            .await
            .is_error
    );
    let result = invoke(
        tools[2].as_ref(),
        r#"{"name":"documentation-review","input":"scope"}"#,
        "start",
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .contains("asynchronous conversational delivery is unavailable")
    );
    let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    let run_id = value["run_id"].as_str().unwrap();
    owner.wait(run_id).await.unwrap();
    let status = invoke(
        tools[3].as_ref(),
        &json!({"run_id":run_id}).to_string(),
        "status",
    )
    .await;
    assert!(status.content.contains("waiting"));
    assert!(status.content.contains("requests"));
    let replay = invoke(
        tools[2].as_ref(),
        r#"{"name":"documentation-review","input":"scope"}"#,
        "start",
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&replay.content).unwrap()["run_id"],
        value["run_id"]
    );
    let missing = Parent::new(
        Ok(controller(
            Arc::new(Store::open_in_memory()),
            crate::workflow::Catalog::default(),
            crate::fixtures::shared_workspace(),
            dir.path(),
        )),
        "source".into(),
        false,
        std::sync::Arc::new(otto_core::agent::budget::SessionBudgetCell::new(
            otto_core::agent::budget::untimed(),
        )),
        Default::default(),
    );
    let missing_tools = missing.tools();
    let arguments = RawValue::from_string(r#"{"name":"removed","input":"scope"}"#.into()).unwrap();
    assert!(
        missing_tools[2]
            .approval_spec(&arguments)
            .unwrap()
            .is_none()
    );
    assert!(
        invoke(missing_tools[2].as_ref(), arguments.get(), "new-operation")
            .await
            .is_error,
        "missing definition must never bypass admission to create a new run"
    );
    let huge = bounded(json!({"output":"\u{0001}".repeat(100000)}));
    assert!(
        huge.content.len() <= 65536,
        "whole tool output hard bound must survive JSON escaping"
    );
    assert!(huge.content.contains("truncated"));
    owner.close().await;
}
#[tokio::test]
async fn non_parent_service_guard_cannot_be_bypassed_by_direct_controller_call() {
    let (dir, catalog) = fixture();
    let store = Arc::new(Store::open_in_memory());
    let owner = controller(
        store.clone(),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let result =
        crate::subagent::provenance::non_parent(owner.start("documentation-review", "scope")).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("children and workflow steps")
    );
    let result = crate::subagent::provenance::non_parent(owner.start_parent(
        "documentation-review",
        "scope",
        "forged",
        "op",
        otto_core::agent::budget::RootBudget::new(
            "root",
            Default::default(),
            otto_core::agent::budget::untimed(),
        ),
    ))
    .await;
    assert!(result.is_err());
    assert!(
        store
            .list_runs(
                crate::fixtures::shared_workspace(),
                &crate::workflow::ListQuery::all()
            )
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn fake_provider_conversation_discovers_inspects_starts_and_preserves_human_gate() {
    let (dir, catalog) = fixture();
    let store = Arc::new(Store::open_in_memory());
    let owner = controller(
        store.clone(),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let source = session("source");
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("list", "workflow_list", "{}", Usage::default()),
            assistant_tool_call(
                "inspect",
                "workflow_inspect",
                r#"{"name":"documentation-review"}"#,
                Usage::default(),
            ),
            assistant_tool_call(
                "start",
                "workflow_start",
                r#"{"name":"documentation-review","input":"installation instructions"}"#,
                Usage::default(),
            ),
            assistant_text(
                "Started the review; the acceptance gate remains human-owned.",
                Usage::default(),
            ),
        ],
    );
    let parent = Parent::new(
        Ok(owner.clone()),
        source.header().id.clone(),
        true,
        std::sync::Arc::new(otto_core::agent::budget::SessionBudgetCell::new(
            otto_core::agent::budget::untimed(),
        )),
        Default::default(),
    );
    let runner = crate::runtime::builder::Runner::scripted_with_tools(
        source.clone(),
        provider.clone(),
        Arc::new(crate::subagent::tasks::Tasks::new()),
        parent.tools(),
    );
    runner
        .run(
            "Run the documented review",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 4);
    let runs = store
        .origin_runs(crate::fixtures::shared_workspace(), "source")
        .unwrap();
    assert_eq!(runs.len(), 1);
    let run = owner.wait(&runs[0]).await.unwrap();
    assert_eq!(run.status, RunStatus::Waiting);
    let requests = owner.requests(&run.id).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status, "pending");
    assert!(
        source
            .messages()
            .iter()
            .flat_map(|m| &m.blocks)
            .any(|block| block.block_type == BlockType::ToolResult
                && block.tool_call_id == "start"
                && block.text.contains(&run.id))
    );
    owner.close().await;
    runner.close();
}
#[test]
fn policy_is_conditional_and_contains_six_schema_correct_examples() {
    let parent = Parent::new(
        Err("not needed".into()),
        "source".into(),
        false,
        std::sync::Arc::new(otto_core::agent::budget::SessionBudgetCell::new(
            otto_core::agent::budget::untimed(),
        )),
        Default::default(),
    );
    let definitions = parent
        .tools()
        .iter()
        .map(|t| t.definition())
        .collect::<Vec<_>>();
    let prompt = crate::runtime::prompt::system_prompt_for(
        &definitions,
        Default::default(),
        "openai-compatible",
        "",
        "",
    );
    assert!(prompt.contains("<workflow_selection>"));
    for number in 1..=6 {
        assert!(
            prompt.contains(&format!("{number}. User:"))
                || (number == 5 && prompt.contains("5. Tool"))
        );
    }
    let absent = crate::runtime::prompt::system_prompt_for(
        &definitions[..3],
        Default::default(),
        "openai-compatible",
        "",
        "",
    );
    assert!(!absent.contains("<workflow_selection>"));
}
