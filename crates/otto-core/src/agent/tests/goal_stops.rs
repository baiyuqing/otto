use super::*;
use crate::goal::{Execution, Goal, Status};

#[tokio::test]
async fn goal_stop_distinguishes_user_cancellation_from_runtime_interruption() {
    for (reason, expected) in [
        (OperationStopReason::UserCancellation, Execution::Paused),
        (OperationStopReason::Shutdown, Execution::Interrupted),
        (OperationStopReason::Migration, Execution::Interrupted),
        (OperationStopReason::TransportLost, Execution::Interrupted),
        (OperationStopReason::ProcessLost, Execution::Interrupted),
    ] {
        let session = MemorySession::new();
        let mut goal = Goal::draft("Verify the export".into());
        goal.status = Status::Open;
        goal.execution = Execution::Ready;
        goal.criteria = vec!["Export tests pass".into()];
        assert!(session.compare_and_set_goal(None, &goal).await.unwrap());
        let agent = Agent::new(
            ScriptedProvider::new(vec![]),
            EchoExecutor,
            session,
            test_options(),
        );
        agent
            .record_goal_stop(
                &AgentError::Provider(ProviderError::Cancelled),
                Some(reason),
            )
            .await
            .unwrap();
        assert_eq!(agent.session().goal().unwrap().execution, expected);
    }
}
