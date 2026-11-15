//! Session-owned human command execution; never creates a model turn.
use super::{Server, internal_error, json_response, not_found};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct Invocation {
    name: String,
    #[serde(default)]
    args: String,
}

pub(super) async fn execute(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let invocation: Invocation = match serde_json::from_slice(&body) {
        Ok(invocation) => invocation,
        Err(_) => return super::bad_request("invalid local command invocation"),
    };
    let Some(controller) = server.controller_for_route(&id).await else {
        return not_found("session not found");
    };
    match tokio::task::spawn_blocking(move || {
        let result = controller.memory_command(&invocation.name, &invocation.args);
        let (output, error) = match result {
            Ok(reply) => {
                let error = matches!(&reply,
                    otto::app::management::Reply::MemoryServiceResult {
                        status: otto_core::session::local_command::LocalCommandStatus::Error, ..
                    });
                (otto::app::management::format_management_reply(&reply), error)
            }
            Err(error) if error.kind == otto::app::management::ManagementErrorKind::Internal => {
                return Err(error.message);
            }
            Err(error) => (error.message, true),
        };
        Ok(serde_json::json!({"output": output, "error": error, "local_commands": controller.info().local_commands}))
    }).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, &value),
        Ok(Err(message)) => super::error_response(StatusCode::INTERNAL_SERVER_ERROR, "local_command_recording", &message),
        Err(error) => internal_error(&server.log, &error.to_string()),
    }
}
