
use super::project_invocation_identity;
use otto_core::model::{Block, BlockType, Message, OperationId};
use otto_core::session::{OperationFact, OperationLedger};

#[test]
fn pending_identity_uses_persisted_facts_without_rewriting_prior_call() {
    let call = Block {
        block_type: BlockType::ToolCall,
        tool_call_id: "reused".into(),
        tool_name: "read".into(),
        ..Default::default()
    };
    let result = Block {
        block_type: BlockType::ToolResult,
        tool_call_id: "reused".into(),
        ..Default::default()
    };
    let mut messages = vec![Message {
        blocks: vec![call.clone(), result, call],
        ..Default::default()
    }];
    let mut ledger = OperationLedger::default();
    ledger
        .apply(OperationFact::attempt(
            OperationId::new("op_current").unwrap(),
            1,
            "reused",
            "read",
        ))
        .unwrap();
    project_invocation_identity(&mut messages, &ledger);
    assert!(messages[0].blocks[0].invocation_id.is_empty());
    assert!(messages[0].blocks[1].invocation_id.is_empty());
    assert_eq!(messages[0].blocks[2].invocation_id, "op_current");
    assert_eq!(messages[0].blocks[2].invocation_attempt, Some(1));
    let serialized = serde_json::to_string(&messages).unwrap();
    let decoded: Vec<Message> = serde_json::from_str(&serialized).unwrap();
    assert_eq!(decoded[0].blocks[2].invocation_id, "op_current");
}
