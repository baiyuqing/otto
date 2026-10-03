//! The `session/request_permission` round trip for Bash elevation or persistent sandbox read access.
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

// Option IDs are opaque. Keep the legacy ID; the kind and label describe persistence.
const ALLOW_ONCE: &str = "allow_once";
const REJECT_ONCE: &str = "reject_once";

/// The permission request for elevation or a persistent sandbox read grant.
pub(super) fn permission_params(
    session_id: &str,
    tool_call_id: &str,
    command: &str,
    read_path: &str,
    justification: &str,
) -> Value {
    let (title, kind, label, option_kind) = if read_path.is_empty() {
        (
            command.to_string(),
            ToolKind::Execute,
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )
    } else {
        (
            format!(
                "Permanently allow sandbox read access to {read_path}\nSaved to read_paths; commands stay sandboxed.\nCommand: {command}\nReason: {justification}"
            ),
            ToolKind::Read,
            "Save read access",
            PermissionOptionKind::AllowAlways,
        )
    };
    let tool_call = ToolCallUpdate::new(
        tool_call_id.to_string(),
        ToolCallUpdateFields::new()
            .title(title)
            .kind(kind)
            .raw_input(json!({ "command": command, "read_path": read_path })),
    );
    let request = RequestPermissionRequest::new(
        session_id.to_string(),
        tool_call,
        vec![
            PermissionOption::new(ALLOW_ONCE, label, option_kind),
            PermissionOption::new(REJECT_ONCE, "Deny", PermissionOptionKind::RejectOnce),
        ],
    );
    serde_json::to_value(request).expect("permission request serializes")
}

/// Asks the client for the described permission and waits for the answer.
pub(super) async fn request_permission(
    connection: &Connection,
    session_id: &str,
    tool_call_id: &str,
    command: &str,
    read_path: &str,
    justification: &str,
    cancel: &CancellationToken,
) -> Decision {
    let params = permission_params(session_id, tool_call_id, command, read_path, justification);
    let pending = match &connection.backend {
        super::Backend::Local(local) => local.session(session_id).and_then(|session| {
            session
                .controller
                .pending_approval()
                .map(|(id, _, _)| (session.controller.clone(), id))
        }),
        _ => None,
    };
    let (id, reply) = connection.send_request("session/request_permission", params);
    tokio::select! {
        () = cancel.cancelled() => {
            connection.forget_request(id);
            Decision::Cancelled
        }
        () = async {
            if let Some((controller, approval_id)) = &pending {
                controller.approval_withdrawn(approval_id).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {
            connection.forget_request(id);
            connection.send(serde_json::json!({"jsonrpc":"2.0", "method":"$/cancel_request", "params":{"requestId":id}}));
            Decision::Deny
        },
        reply = reply => {
            let decision = reply.map_or(Decision::Deny, |frame| decide(&frame));
            if decision == Decision::Allow
                && let Some((controller, approval_id)) = &pending
                && controller.reserve_approval(approval_id).is_err() {
                return Decision::Deny;
            }
            decision
        },
    }
}

/// Reads the client's response frame.
pub(super) fn decide(frame: &Value) -> Decision {
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
    fn read_permission_discloses_persistence_and_uses_allow_always() {
        let params = permission_params("s1", "c1", "cat fixture", "/fixture", "read fixture");
        let title = params["toolCall"]["title"].as_str().unwrap();
        assert!(title.contains("Permanently"));
        assert!(title.contains("/fixture"));
        assert!(title.contains("commands stay sandboxed"));
        assert_eq!(params["toolCall"]["kind"], "read");
        assert_eq!(params["options"][0]["kind"], "allow_always");
        assert_eq!(params["options"][0]["name"], "Save read access");
    }

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
