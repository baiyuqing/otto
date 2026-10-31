//! The web delivery routes of scheduled items, and the prompt runner of
//! `otto serve`.
//!
//! The browser polls `POST /v1/schedule/claim`, shows each item, then calls
//! `POST /v1/schedule/{id}/delivered`. A claim holds an item for 120 s; an
//! item that is not confirmed in that time is returned by a later claim.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{
    Factory, Server, WorkspaceLoadError, bad_request, error_response, internal_error,
    json_response, not_found,
};
use crate::schedule::worker::PromptRunner;
use crate::schedule::{RunOutcome, Scope, delivery_text};

const WEB_LEASE: Duration = Duration::from_secs(120);
const CLAIM_LIMIT: usize = 10;

#[derive(Debug, Serialize)]
struct ClaimedItem {
    id: String,
    /// Unix milliseconds; echo it in the `delivered` request.
    due_at: i64,
    text: String,
}

#[derive(Debug, Serialize)]
struct ClaimResponse {
    items: Vec<ClaimedItem>,
}

#[derive(Debug, Deserialize)]
struct DeliveredBody {
    due_at: i64,
}

fn unavailable() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "schedule_unavailable",
        "the schedule store is not available",
    )
}

pub async fn claim(State(server): State<Arc<Server>>) -> Response {
    let Some(store) = &server.schedule else {
        return unavailable();
    };
    let now = Utc::now();
    match store.claim_deliveries(&Scope::Web, WEB_LEASE, CLAIM_LIMIT, now) {
        Ok(items) => json_response(
            StatusCode::OK,
            &ClaimResponse {
                items: items
                    .iter()
                    .map(|item| ClaimedItem {
                        id: item.label(),
                        due_at: item.due_at.timestamp_millis(),
                        text: delivery_text(item, now),
                    })
                    .collect(),
            },
        ),
        Err(error) => internal_error(&server.log, &error.to_string()),
    }
}

pub async fn delivered(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(store) = &server.schedule else {
        return unavailable();
    };
    let Some(number) = crate::schedule::parse_label(&id) else {
        return not_found("schedule item not found");
    };
    let Ok(parsed) = serde_json::from_slice::<DeliveredBody>(&body) else {
        return bad_request("body must be {\"due_at\": <unix ms>}");
    };
    let Some(due_at) = DateTime::<Utc>::from_timestamp_millis(parsed.due_at) else {
        return bad_request("due_at is out of range");
    };
    // `false` means the occurrence was already confirmed or is no longer
    // current; the browser has nothing to retry either way.
    match store.delivered(number, due_at, Utc::now()) {
        Ok(_) => json_response(StatusCode::OK, &serde_json::json!({})),
        Err(error) => internal_error(&server.log, &error.to_string()),
    }
}

/// Runs scheduled prompts in new sessions created through the server's
/// [`Factory`], in the workspace the item was created in.
pub struct FactoryRunner(pub Arc<dyn Factory>);

#[async_trait::async_trait]
impl PromptRunner for FactoryRunner {
    async fn run_item(
        &self,
        item: &crate::schedule::Item,
        text: &str,
        cancel: &CancellationToken,
    ) -> RunOutcome {
        let canonical = match self.0.load_workspace(&item.workspace).await {
            Ok((info, _)) => info.path,
            Err(
                WorkspaceLoadError::Invalid(error)
                | WorkspaceLoadError::NotAdmitted(error)
                | WorkspaceLoadError::Failed(error),
            ) => return RunOutcome::FailedBeforeTools(error),
        };
        let controller = match self.0.create(Some(&canonical)).await {
            Ok(controller) => controller,
            Err(error) => return RunOutcome::FailedBeforeTools(error),
        };
        let _ = crate::app::scheduled::inherit_authority(&controller, item).await;
        let outcome = crate::app::scheduled::run(&controller, text, cancel).await;
        controller.close_resources().await;
        let _ = controller.close();
        outcome
    }
    async fn run(&self, workspace: &str, text: &str, cancel: &CancellationToken) -> RunOutcome {
        let canonical = match self.0.load_workspace(workspace).await {
            Ok((info, _)) => info.path,
            Err(
                WorkspaceLoadError::Invalid(message)
                | WorkspaceLoadError::NotAdmitted(message)
                | WorkspaceLoadError::Failed(message),
            ) => return RunOutcome::Failed(message),
        };
        let controller = match self.0.create(Some(&canonical)).await {
            Ok(controller) => controller,
            Err(error) => return RunOutcome::FailedBeforeTools(error),
        };
        let outcome = crate::app::scheduled::run(&controller, text, cancel).await;
        controller.close_resources().await;
        let _ = controller.close();
        outcome
    }
}
