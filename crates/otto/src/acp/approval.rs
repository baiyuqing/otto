//! The `session/request_permission` round trip for one elevated bash command.
//!
//! The request is sent after the turn that produced it has returned. The wait
//! ends on the client's response, on the session's prompt token, or when the
//! connection stops; a response that arrives after a cancellation finds no
//! pending entry and is dropped by the connection.

use agent_client_protocol_schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::Connection;
use crate::app::ApprovalDecision as Decision;

const ALLOW_ONCE: &str = "allow_once";
const REJECT_ONCE: &str = "reject_once";

/// Asks the client to allow `command` once and waits for the answer.
pub(super) async fn request_permission(
    connection: &Connection,
    session_id: &str,
    tool_call_id: &str,
    command: &str,
    cancel: &CancellationToken,
) -> Decision {
    let tool_call = ToolCallUpdate::new(
        tool_call_id.to_string(),
        ToolCallUpdateFields::new()
            .title(command.to_string())
            .kind(ToolKind::Execute)
            .raw_input(json!({ "command": command })),
    );
    let request = RequestPermissionRequest::new(
        session_id.to_string(),
        tool_call,
        vec![
            PermissionOption::new(ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new(REJECT_ONCE, "Deny", PermissionOptionKind::RejectOnce),
        ],
    );
    let params = serde_json::to_value(request).expect("permission request serializes");
    let (id, reply) = connection.send_request("session/request_permission", params);
    tokio::select! {
        () = cancel.cancelled() => {
            connection.forget_request(id);
            Decision::Cancelled
        }
        reply = reply => reply.map_or(Decision::Deny, |frame| decide(&frame)),
    }
}

/// Reads the client's response frame.
fn decide(frame: &Value) -> Decision {
    let Some(result) = frame.get("result") else {
        return Decision::Deny;
    };
    match serde_json::from_value::<RequestPermissionResponse>(result.clone()) {
        Ok(response) => match response.outcome {
            RequestPermissionOutcome::Selected(selected)
                if &*selected.option_id.0 == ALLOW_ONCE =>
            {
                Decision::Allow
            }
            _ => Decision::Deny,
        },
        Err(_) => Decision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_selected_allow_once_allows() {
        let selected =
            |id: &str| json!({"result": {"outcome": {"outcome": "selected", "optionId": id}}});
        assert_eq!(decide(&selected("allow_once")), Decision::Allow);
        assert_eq!(decide(&selected("reject_once")), Decision::Deny);
        assert_eq!(decide(&selected("other")), Decision::Deny);
        assert_eq!(
            decide(&json!({"result": {"outcome": {"outcome": "cancelled"}}})),
            Decision::Deny
        );
        assert_eq!(
            decide(&json!({"error": {"code": -32603, "message": "x"}})),
            Decision::Deny
        );
        assert_eq!(decide(&json!({"result": {"nope": 1}})), Decision::Deny);
    }
}
