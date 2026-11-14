//! Request id, logging and metrics middleware plus shared turn error constants.

use super::*;

pub(super) const TURN_ACTIVE: &str = "turn already active";
pub(super) const PROMPT_NOT_PERSISTED: &str = app::turn_queue::PROMPT_NOT_PERSISTED;
pub(super) const QUEUE_FULL: &str = "turn queue full";

/// 16 random bytes, hex encoded. The same generator as `cmd`'s, duplicated
/// because the package boundary forbids the import.
pub(super) fn new_id() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

// ---- middleware ----

/// Re-opens a session the idle sweeper evicted before the route runs, so a
/// client that holds a session id keeps working across an eviction. Runs inside
/// [`instrument`] and [`authorization::authorize_operation`], so a request
/// still has to pass the token gate and authorization first.
///
/// A failure is not answered here: the route resolves the session itself and
/// answers its own 404 (or its own error) exactly as it does for a session that
/// was never open.
pub(super) async fn reopen_evicted_session(
    State(server): State<Arc<Server>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(id) = session_id_in_path(request.uri().path()) {
        let _ = server.session_for_route(id).await;
    }
    next.run(request).await
}

/// The session id in `/v1/sessions/{id}[...]`, when the request's path names
/// one. Anything else -- `/v1/sessions` itself, a route that is not
/// session-scoped, an id that is not a session id -- is left to the route.
pub(super) fn session_id_in_path(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/v1/sessions/")?.split('/').next()?;
    otto::session::is_session_id(id).then_some(id)
}

/// Request-ID handling, token gating, structured logging and HTTP metrics.
/// Token gating is folded in so a 401 is still logged and measured under its
/// real route. The token gates `/v1/` routes; when the TCP listener is
/// non-loopback it gates every route except `GET /healthz`, including
/// `/metrics` and unmatched paths. A request that already carries a
/// [`authorization::Principal`] ([`Server::router_for`]) is never gated. A
/// request with the valid token, or any request when the server has no token,
/// is attached [`authorization::Principal::Owner`]. The `http_request` log
/// line carries `principal` when the request has one and `peer` when the
/// listener attached the peer address.
pub(super) async fn instrument(
    State(server): State<Arc<Server>>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let mut id = request_id(
        request
            .headers()
            .get("x-request-id")
            .map(header::HeaderValue::as_bytes)
            .unwrap_or_default(),
    );
    if id.is_empty() {
        id = new_id().unwrap_or_default();
    }

    let method = request.method().clone();
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string());
    let route = route_label(&method, matched.as_deref());

    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<crate::listen::PeerAddress>>()
        .map(|info| info.0.0.to_string());
    let gated_route =
        route.contains(" /v1/") || (server.remote_listener && route != "GET /healthz");

    let start = Instant::now();
    if request
        .extensions()
        .get::<authorization::Principal>()
        .is_none()
        && (server.token.is_empty() || auth::authorized(&server.token, request.headers()))
    {
        request
            .extensions_mut()
            .insert(authorization::Principal::Owner);
    }
    let principal = request
        .extensions()
        .get::<authorization::Principal>()
        .map(|principal| principal.name().to_string());
    request
        .extensions_mut()
        .insert(authorization::RequestId(id.clone()));
    let mut response = if principal.is_none() && gated_route {
        auth::unauthorized()
    } else {
        next.run(request).await
    };
    let elapsed = start.elapsed();

    if let Ok(value) = header::HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }

    let status = response.status().as_u16();
    server
        .metrics
        .http_request(&route, method.as_str(), status, elapsed);
    let mut fields: Vec<(&str, String)> = vec![
        ("method", method.to_string()),
        ("route", route),
        ("status", status.to_string()),
        ("duration_ms", elapsed.as_millis().to_string()),
        ("request_id", id),
    ];
    if let Some(principal) = principal {
        fields.push(("principal", principal));
    }
    if let Some(peer) = peer {
        fields.push(("peer", peer));
    }
    server.log.info("http_request", &fields);
    response
}

/// The metric and log label for one request. axum's [`MatchedPath`] carries
/// only the path, so the method is prepended.
pub(super) fn route_label(method: &Method, matched: Option<&str>) -> String {
    match matched {
        None => "unmatched".to_string(),
        Some(path) => format!("{method} {path}"),
    }
}
