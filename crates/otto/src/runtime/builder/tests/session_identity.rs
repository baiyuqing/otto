use super::*;
use crate::fixtures::subagent_doubles::{
    FakeProvider, assistant_text, assistant_tool_call, match_any,
};
use otto_core::model::BlockType;

#[tokio::test]
async fn session_info_main_runner_binds_actual_persisted_and_no_session_identity() {
    for no_session in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new();
        provider.add_route(
            match_any,
            vec![
                assistant_tool_call("identity-one", "session_info", "{}", Usage::default()),
                assistant_tool_call("identity-two", "session_info", "{}", Usage::default()),
                assistant_text("done", Usage::default()),
            ],
        );
        let builder = builder_for(shared_with_no_session(dir.path(), no_session), dir.path());
        TEST_PROVIDERS
            .lock()
            .unwrap()
            .push((builder.home.clone(), provider));
        let session = builder.create_session(&runtime()).unwrap();
        let expected_id = session.header().id;
        assert!(otto_session::is_session_id(&expected_id));
        assert_eq!(session.session_id().as_deref(), Some(expected_id.as_str()));
        let runner = builder.build_runner(&session, &runtime()).await.unwrap();
        let cancel = CancellationToken::new();
        runner.run("identify", &mut |_| {}, &cancel).await.unwrap();
        let results = session
            .messages()
            .into_iter()
            .flat_map(|message| message.blocks)
            .filter(|block| {
                block.block_type == BlockType::ToolResult && block.tool_name == "session_info"
            })
            .map(|block| serde_json::from_str::<serde_json::Value>(&block.text).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results,
            vec![serde_json::json!({"session_id": expected_id}); 2]
        );
        if no_session {
            assert!(session.path().is_empty());
            assert!(
                !dir.path().join("sessions").exists(),
                "ephemeral identity must not create a session file"
            );
        } else {
            assert_eq!(Store::read_header(session.path()).unwrap().id, expected_id);
        }
        session.close_resources().await;
        runner.close();
        TEST_PROVIDERS
            .lock()
            .unwrap()
            .retain(|(home, _)| home != &builder.home);
    }
}
