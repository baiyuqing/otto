//! Turn start, status, event stream and cancel routes.

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct StartTurnBody {
    #[serde(default)]
    client_source: Option<otto_core::model::InputSource>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    image: Option<StartTurnImage>,
    /// Queue behind a running turn instead of answering 409 `turn_active`.
    #[serde(default)]
    queue: bool,
    /// Explicit follow-up intent, even if admitted while idle. No source authority.
    #[serde(default)]
    queued_input: bool,
    /// Per-turn chat channel context.
    #[serde(default)]
    channel: Option<Channel>,
    /// Where the turn came from: `terminal`, `web` or
    /// `chat:<platform>:<chat id>`. Absent means `web`.
    #[serde(default)]
    origin: Option<String>,
    /// The client's stable id for this input, so a retry is not run twice.
    #[serde(default)]
    input_id: Option<String>,
}

/// The `400` message for a malformed `input_id`.
pub const INVALID_INPUT_ID: &str =
    "input_id must be 1 to 128 ASCII letters, digits, '.', '_', ':' or '-'";

#[derive(Debug, Deserialize)]
pub(super) struct StartTurnImage {
    #[serde(default)]
    data: String,
    #[serde(default)]
    mime_type: String,
}

/// The client-supplied `X-Request-Id` header, sanitized; empty when absent
/// (the middleware's generated fallback is not read here).
pub(super) fn client_request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .map(|value| request_id(value.as_bytes()))
        .unwrap_or_default()
}

pub(super) async fn start_turn(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let request_id = client_request_id(&headers);
    // Unlike create and rename, the body is decoded unconditionally here, so an
    // empty body is a 400.
    let parsed: StartTurnBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return bad_request("invalid JSON body"),
    };
    if parsed.text.trim().is_empty() && parsed.image.is_none() {
        return bad_request("text must not be empty");
    }
    let image = parsed
        .image
        .map(|image| Block::image(image.data, image.mime_type));
    if image
        .as_ref()
        .is_some_and(|image| image.validate().is_err())
    {
        return bad_request("image is invalid");
    }
    let origin = match parsed.origin.as_deref().map(otto::schedule::Origin::parse) {
        Some(Ok(origin)) => origin,
        Some(Err(message)) => return bad_request(&message),
        None => otto::schedule::Origin::web(),
    };
    if let (Some(channel), Some(platform)) = (parsed.channel, origin.chat_platform()) {
        let expected = match channel {
            Channel::Telegram => "telegram",
            Channel::Feishu => "feishu",
        };
        if platform != expected {
            return bad_request("origin platform does not match channel");
        }
    }
    let stream = parsed.stream.unwrap_or(true);

    if parsed
        .input_id
        .as_deref()
        .is_some_and(|id| !otto_core::model::is_input_id(id))
    {
        return bad_request(INVALID_INPUT_ID);
    }
    let turn = match server.start_turn_with_provenance(
        &session,
        parsed.text,
        image,
        (parsed.channel, Some(origin)),
        parsed.queue,
        &request_id,
        parsed.input_id,
        Some({
            let mut provenance = otto_core::model::InputProvenance::remote(
                otto_core::model::InputTransport::Http,
                parsed.client_source,
            );
            if parsed.queued_input {
                provenance.delivery = otto_core::model::InputDelivery::Queued;
            }
            provenance
        }),
    ) {
        Ok(turn) => turn,
        Err(error) if error == DUPLICATE_INPUT => {
            return error_response(StatusCode::CONFLICT, "duplicate_input", DUPLICATE_INPUT);
        }
        Err(error) if error == TURN_ACTIVE => {
            return turn_active("a turn is already active for this session");
        }
        Err(error) if error == QUEUE_FULL => {
            return error_response(
                StatusCode::CONFLICT,
                "queue_full",
                "the turn queue for this session is full",
            );
        }
        Err(error) if error == PROMPT_NOT_PERSISTED => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "input_not_persisted",
                PROMPT_NOT_PERSISTED,
            );
        }
        Err(error) => return internal_error(&server.log, &error),
    };

    let mut response = if stream {
        stream_sse(&server.metrics, Arc::clone(&turn), 0)
    } else {
        wait_done(&turn).await;
        json_response(StatusCode::OK, &turn.summary())
    };
    if let Ok(value) = axum::http::HeaderValue::from_str(&turn.id) {
        response.headers_mut().insert(TURN_ID_HEADER, value);
    }
    response
}

/// Names the turn a `POST .../turns` response belongs to.
pub(super) const TURN_ID_HEADER: &str = "otto-turn-id";

/// Blocks until the turn is done, riding the same version broadcast the SSE
/// reader uses.
pub(super) async fn wait_done(turn: &Arc<Turn>) {
    let mut changed = turn.subscribe();
    while !turn.is_done() {
        if changed.changed().await.is_err() {
            return;
        }
    }
}

/// The turn named by the path, or the 404 message to answer with.
pub(super) fn resolve_turn(
    server: &Server,
    id: &str,
    turn_id: &str,
) -> Result<Arc<Turn>, &'static str> {
    let Some(session) = server.lookup(id) else {
        return Err("session not found");
    };
    session.find_turn(turn_id).ok_or("turn not found")
}

pub(super) async fn get_turn(
    State(server): State<Arc<Server>>,
    Path((id, turn_id)): Path<(String, String)>,
) -> Response {
    match resolve_turn(&server, &id, &turn_id) {
        Ok(turn) => json_response(StatusCode::OK, &turn.summary()),
        Err(message) => not_found(message),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct EventsQuery {
    after: Option<String>,
}

pub(super) async fn turn_events(
    State(server): State<Arc<Server>>,
    Path((id, turn_id)): Path<(String, String)>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Response {
    let turn = match resolve_turn(&server, &id, &turn_id) {
        Ok(turn) => turn,
        Err(message) => return not_found(message),
    };

    let after = match query.after.as_deref().filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<usize>() {
            Ok(n) => n + 1,
            Err(_) => return bad_request("after must be a non-negative integer"),
        },
        None => headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse::<usize>().ok())
            .map(|n| n + 1)
            .unwrap_or(0),
    };

    stream_sse(&server.metrics, turn, after)
}

pub(super) async fn cancel_turn(
    State(server): State<Arc<Server>>,
    Path((id, turn_id)): Path<(String, String)>,
) -> Response {
    match resolve_turn(&server, &id, &turn_id) {
        Ok(turn) => {
            // A queued turn never ran: remove it and end it `canceled`. The
            // queue and the end happen under one lock so `start_next` cannot
            // pick it up in between.
            let removed = server.lookup(&id).is_some_and(|session| {
                let mut guard = session.lock();
                let state = &mut *guard;
                let mut cancelled = Vec::new();
                for queue in [&mut state.queue, &mut state.restored] {
                    if let Some(position) =
                        queue.iter().position(|queued| queued.turn.id == turn.id)
                        && let Some(queued) = queue.remove(position)
                    {
                        cancelled.push(queued);
                    }
                }
                for queued in &cancelled {
                    turn.finish(Some(String::new()), true);
                    // Cancelled for good: a restart must not run it.
                    if let (Some(input_id), Some(journal)) =
                        (&queued.input_id, session.ctrl.turn_queue())
                    {
                        journal.forget(input_id);
                    }
                }
                !cancelled.is_empty()
            });
            if removed {
                server.bump_status();
            } else {
                turn.cancel();
            }
            Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::empty())
                .expect("static response")
        }
        Err(message) => not_found(message),
    }
}

#[cfg(test)]
#[path = "tests/input_provenance_cases.rs"]
mod provenance_tests;
