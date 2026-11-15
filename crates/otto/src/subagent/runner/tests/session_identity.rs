//! Runner-local identities, including concurrent and resumed child execution.
use super::*;

fn identity_responses(tasks: &Tasks, id: &str) -> Vec<String> {
    tasks
        .history(id)
        .unwrap()
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| {
            block.block_type == BlockType::ToolResult && block.tool_name == "session_info"
        })
        .map(|block| {
            let value: serde_json::Value =
                serde_json::from_str(&block.text).expect("identity JSON");
            assert_eq!(
                value.as_object().unwrap().len(),
                1,
                "no paths or extra fields"
            );
            value["session_id"].as_str().unwrap().to_owned()
        })
        .collect()
}

fn identity_route(provider: &FakeProvider, prompt: &str) {
    provider.add_route(
        match_prompt(prompt),
        vec![
            assistant_tool_call("identity-1", "session_info", "{}", Usage::default()),
            assistant_tool_call("identity-2", "session_info", "{}", Usage::default()),
            assistant_text("done", Usage::default()),
        ],
    );
}

#[tokio::test]
async fn session_info_concurrent_children_have_distinct_stable_own_ids() {
    for persisted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new();
        identity_route(&provider, "first");
        identity_route(&provider, "second");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let arrivals = Arc::new(AtomicUsize::new(0));
        provider.set_hook(Arc::new(move |_, _| {
            let barrier = barrier.clone();
            let wait = arrivals.fetch_add(1, Ordering::SeqCst) < 2;
            Box::pin(async move {
                if wait {
                    barrier.wait().await;
                }
            })
        }));
        let tasks = Arc::new(Tasks::new());
        let parent_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        // A parent-bound catalog entry must never leak through to a child.
        let mut config = test_config(
            &provider,
            &tasks,
            vec![Box::new(crate::tool::session_info::SessionInfo {
                session_id: parent_id.into(),
            })],
        );
        let operation_counter = Arc::new(AtomicUsize::new(0));
        config.template.new_operation_id = Arc::new(move || {
            otto_core::model::OperationId::new(format!(
                "identity_op_{}",
                operation_counter.fetch_add(1, Ordering::SeqCst)
            ))
            .map_err(|error| error.to_string())
        });
        if persisted {
            let parent = dir.path().join("parent.jsonl");
            config.child_session = Some(Arc::new(move |task_id, session_id| {
                let store = otto_session::Store::create_child_lazy(
                    &parent,
                    &format!("{task_id}-{session_id}"),
                    otto_core::session::Header {
                        id: session_id.into(),
                        workspace: parent.parent().unwrap().to_string_lossy().into_owned(),
                        provider: "openai-compatible".into(),
                        model: "test-model".into(),
                        created_at: chrono::Utc::now(),
                        ..Default::default()
                    },
                )
                .map_err(|error| error.to_string())?;
                let path =
                    otto_session::Store::child_path(&parent, &format!("{task_id}-{session_id}"))
                        .to_string_lossy()
                        .into_owned();
                Ok(Some((Arc::new(store) as Transcript, path)))
            }));
        }
        let (runner, _) = runner(config);
        let first = runner
            .start(StartRequest {
                prompt: "first".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let second = runner
            .start(StartRequest {
                prompt: "second".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let first = wait_final(&tasks, &first.id).await;
        let second = wait_final(&tasks, &second.id).await;
        assert_eq!(first.status, TaskStatus::Succeeded, "{first:?}");
        assert_eq!(second.status, TaskStatus::Succeeded, "{second:?}");
        let first_ids = identity_responses(&tasks, &first.id);
        let second_ids = identity_responses(&tasks, &second.id);
        assert_eq!(first_ids.len(), 2);
        assert_eq!(second_ids.len(), 2);
        assert_eq!(first_ids[0], first_ids[1]);
        assert_eq!(second_ids[0], second_ids[1]);
        assert_ne!(first_ids[0], second_ids[0]);
        for (task, ids) in [(&first, &first_ids), (&second, &second_ids)] {
            assert_ne!(ids[0], parent_id);
            assert!(otto_session::is_session_id(&ids[0]));
            if persisted {
                let header = otto_session::Store::read_header(&task.session_path).unwrap();
                assert_eq!(header.id, ids[0]);
            } else {
                assert!(task.session_path.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn session_info_resume_binds_existing_child_header_identity() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent.jsonl");
    let path = interrupted_fixture(
        &parent,
        "t1",
        "original",
        None,
        true,
        true,
        interrupted::INTERRUPTED_STATUS,
    );
    // Do not open the interrupted store before resume: Store::open repairs
    // dangling turns and would change what the resume scanner observes.
    let actual_id = "child".to_owned();
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("resume-identity", "session_info", "{}", Usage::default()),
            assistant_text("done", Usage::default()),
        ],
    );
    let tasks = Arc::new(Tasks::new());
    let config = config_with_children_dir(&provider, &tasks, parent.with_extension(""));
    let (runner, _) = runner(config);
    runner.resume("t1", "continue".into()).await.unwrap();
    let task = wait_final(&tasks, "t1").await;
    assert_eq!(task.status, TaskStatus::Succeeded, "{task:?}");
    assert_eq!(identity_responses(&tasks, "t1"), vec![actual_id.clone()]);
    let header = otto_session::Store::read_header(&path).unwrap();
    assert_eq!(header.id, actual_id);
}
