//! `POST /v1/sessions/{id}/sandbox/changes`: one `[sandbox]` configuration
//! change (`/sandbox allow`, `/sandbox network`, `/sandbox exclude`) written to
//! the serve process's configuration file and applied to every loaded
//! workspace.
//!
//! The change reloads the sandbox executor, so it is refused while any session
//! has a turn in flight, for the same reason as `POST /v1/sandbox/reload`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use otto::sandbox::config_edit::SandboxChange;
use serde::Deserialize;

use super::sandbox::reload_body;
use super::{Server, error_response, json_response, not_found};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeBody {
    change: String,
    value: String,
}

fn invalid_change(message: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, "invalid_sandbox_change", message)
}

pub async fn apply(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let Ok(input) = serde_json::from_slice::<ChangeBody>(&body) else {
        return invalid_change("expected change and value");
    };
    let value = input.value.trim().to_string();
    if value.is_empty() {
        return invalid_change("value must not be empty");
    }
    let change = match input.change.as_str() {
        "allow_read_path" => match session.ctrl.resolve_sandbox_read_path(&value) {
            Ok(path) => SandboxChange::AllowReadPath(path),
            Err(message) => return invalid_change(&message),
        },
        "network" if value == "allow" || value == "deny" => SandboxChange::Network(value),
        "network" => return invalid_change("network must be \"allow\" or \"deny\""),
        "exclude_command" => SandboxChange::ExcludeCommand(value),
        _ => {
            return invalid_change(
                "change must be \"allow_read_path\", \"network\" or \"exclude_command\"",
            );
        }
    };
    if !server.factory.sandbox_reload_available() {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            "sandbox changes are not available",
        );
    }
    if server.any_turn_active() {
        return error_response(
            StatusCode::CONFLICT,
            "turn_active",
            "a turn is active; a sandbox change would replace a running command's sandbox",
        );
    }
    let info = match session.ctrl.amend_sandbox(change.clone()).await {
        Ok(info) => info,
        Err(message) => {
            return error_response(StatusCode::CONFLICT, "sandbox_change_failed", &message);
        }
    };
    let followup = change.followup_prompt(info.summary());
    let others = server.factory.reload_other_sandboxes().await;
    let mut body = match reload_body(&server, Ok(info), others) {
        Ok(body) => body,
        Err(message) => {
            return error_response(StatusCode::CONFLICT, "sandbox_reload_failed", &message);
        }
    };
    // The sandbox object already holds `summary`, its own summary line, so the
    // change's description is `change_summary`.
    body["change_summary"] = change.summary().into();
    body["config_path"] = session.ctrl.config_path().display().to_string().into();
    body["followup"] = followup.into();
    json_response(StatusCode::OK, &body)
}
