//! Cursor-based replies from ordinary wake turns.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, json_response, not_found};

#[derive(Default, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    after: u64,
}

#[derive(Serialize)]
struct WakeReply {
    id: u64,
    text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    presentation_origin: String,
}

#[derive(Serialize)]
struct WakeReplies {
    notices: Vec<WakeReply>,
    last: u64,
}

pub async fn list(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Query(cursor): Query<Cursor>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let (replies, last) = session.ctrl.wake_replies_since(cursor.after);
    json_response(
        StatusCode::OK,
        &WakeReplies {
            notices: replies
                .into_iter()
                .map(|(id, text)| WakeReply {
                    id,
                    text,
                    presentation_origin: String::new(),
                })
                .collect(),
            last,
        },
    )
}
