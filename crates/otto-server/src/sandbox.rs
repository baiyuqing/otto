//! `POST /v1/sandbox/reload`.
//!
//! One sandbox serves every open session, so the reload is refused while any
//! session has a turn in flight: replacing the executor while a `bash` command
//! is using it would otherwise block until that command ends.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use otto::sandbox::info::SandboxInfo;
use serde::Serialize;

use super::{SandboxWire, Server, error_response, json_response, sandbox_wire};

/// One workspace's own reload outcome, part of the additive `workspaces`
/// field described on [`SandboxReloadWire`].
#[derive(Serialize)]
struct WorkspaceReloadWire {
    workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<SandboxWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The multi-workspace success response. `#[serde(flatten)]` keeps the
/// startup workspace's own `mode`/`network`/`bash_available`/`summary` at the
/// top level, exactly where a single-workspace deployment always had them;
/// `workspaces` is the only new field, one entry per host that was reloaded.
#[derive(Serialize)]
struct SandboxReloadWire {
    #[serde(flatten)]
    sandbox: SandboxWire,
    workspaces: Vec<WorkspaceReloadWire>,
}

pub async fn reload(State(server): State<Arc<Server>>) -> Response {
    if !server.factory.sandbox_reload_available() {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            "sandbox reload is not available",
        );
    }
    if server.any_turn_active() {
        return error_response(
            StatusCode::CONFLICT,
            "turn_active",
            "a turn is active; sandbox reload would replace a running command's sandbox",
        );
    }
    let startup_result = match server.factory.reload_sandbox().await {
        None => {
            return error_response(
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                "sandbox reload is not available",
            );
        }
        Some(result) => result,
    };
    reload_response(&server, startup_result).await
}

/// Reloads every other loaded workspace's sandbox and builds the response for
/// the startup workspace's `startup_result`; shared by the reload route and
/// the sandbox changes route.
pub(super) async fn reload_response(
    server: &Server,
    startup_result: Result<SandboxInfo, String>,
) -> Response {
    let others = server.factory.reload_other_sandboxes().await;
    match reload_body(server, startup_result, others) {
        Ok(body) => json_response(StatusCode::OK, &body),
        Err(message) => error_response(StatusCode::CONFLICT, "sandbox_reload_failed", &message),
    }
}

/// The success body of a reload: the startup workspace's sandbox, plus
/// `workspaces` when another workspace has a sandbox of its own. `Err` carries
/// the `sandbox_reload_failed` message.
pub(super) fn reload_body(
    server: &Server,
    startup_result: Result<SandboxInfo, String>,
    others: Vec<(String, Result<SandboxInfo, String>)>,
) -> Result<serde_json::Value, String> {
    // No other workspace has a sandbox of its own: today's exact response,
    // byte for byte, for the single-workspace deployment.
    if others.is_empty() {
        return startup_result
            .map(|info| serde_json::to_value(sandbox_wire(&info)).expect("sandbox wire"));
    }

    let mut workspaces = Vec::with_capacity(1 + others.len());
    let mut failed = Vec::new();
    let mut reloaded = Vec::new();
    for (workspace, result) in
        std::iter::once((server.info.workspace.clone(), startup_result)).chain(others)
    {
        match &result {
            Ok(_) => reloaded.push(workspace.clone()),
            Err(error) => failed.push(format!("{workspace}: {error}")),
        }
        workspaces.push(WorkspaceReloadWire {
            sandbox: result.as_ref().ok().map(sandbox_wire),
            error: result.err(),
            workspace,
        });
    }

    if !failed.is_empty() {
        return Err(format!(
            "sandbox reload failed for {}; reloaded: {}",
            failed.join(", "),
            if reloaded.is_empty() {
                "none".to_string()
            } else {
                reloaded.join(", ")
            }
        ));
    }

    let sandbox = workspaces[0]
        .sandbox
        .clone()
        .expect("every workspace reloaded above");
    Ok(serde_json::to_value(SandboxReloadWire {
        sandbox,
        workspaces,
    })
    .expect("sandbox reload wire"))
}
