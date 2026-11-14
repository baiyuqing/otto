//! Authorization of API operations: the registry covers every route, a
//! non-owner principal is refused before any handler runs, and every
//! transport attaches the owner.

use std::collections::HashSet;

use super::*;
use crate::authorization::{OperationId, Principal};

fn client() -> Principal {
    Principal::Client {
        name: "test-client".to_string(),
    }
}

/// `operation`'s path with every parameter filled in, `session` for `{id}`.
fn concrete_path(operation: OperationId, session: &str) -> String {
    let mut path = operation.path().replace("{id}", session);
    for parameter in [
        "{name}",
        "{span_id}",
        "{message_id}",
        "{approval_id}",
        "{turn_id}",
        "{task_id}",
        "{timer_id}",
        "{record_id}",
        "{candidate_id}",
        "{parent_session}",
    ] {
        path = path.replace(parameter, "nope");
    }
    assert!(!path.contains('{'), "unfilled parameter in {path}");
    path
}

async fn send_as(
    harness: &Harness,
    principal: Principal,
    method: &Method,
    path: &str,
) -> (StatusCode, String) {
    let body = if matches!(*method, Method::POST | Method::PUT | Method::PATCH) {
        Body::from("{}")
    } else {
        Body::empty()
    };
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body)
        .expect("request");
    let response = harness
        .server
        .router_for(principal)
        .oneshot(request)
        .await
        .expect("infallible");
    let status = response.status();
    // `/v1/status` streams forever when served; a refusal is a short body.
    let bytes = if status == StatusCode::OK && path == "/v1/status" {
        Bytes::new()
    } else {
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body")
    };
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn the_router_registers_every_operation_exactly_once() {
    let routes = api_routes();
    let registered = routes.registered();
    let unique: HashSet<OperationId> = registered.iter().copied().collect();
    assert_eq!(
        unique.len(),
        registered.len(),
        "an operation is registered twice"
    );
    let all: HashSet<OperationId> = OperationId::ALL.iter().copied().collect();
    assert_eq!(unique, all);
}

#[test]
fn operation_method_and_path_pairs_are_unique() {
    let mut seen = HashSet::new();
    for operation in OperationId::ALL {
        assert!(
            seen.insert((operation.method(), operation.path())),
            "{} repeats {} {}",
            operation.name(),
            operation.method(),
            operation.path()
        );
    }
}

#[tokio::test]
async fn a_client_principal_is_refused_every_operation_before_its_handler_runs() {
    let harness = Harness::new();
    let session = harness.create().await;
    for operation in OperationId::ALL {
        if !operation.requires_principal() {
            continue;
        }
        let path = concrete_path(*operation, &session);
        let (status, body) = send_as(&harness, client(), &operation.method(), &path).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{} {path}: {body}",
            operation.method()
        );
        let error: Value = serde_json::from_str(&body).expect("error body");
        assert_eq!(error["error"]["code"], "forbidden", "{path}");
    }
    // `POST /v1/sessions` was refused, so the one session is still the only one.
    let list = harness.send("GET", "/v1/sessions", None).await.json();
    assert_eq!(list["sessions"].as_array().expect("rows").len(), 1);
    assert!(
        harness
            .logged()
            .contains("msg=authorization_denied operation=SessionCreate principal=test-client")
    );
}

#[tokio::test]
async fn head_is_authorized_as_get() {
    let harness = Harness::new();
    let (status, _) = send_as(&harness, client(), &Method::HEAD, "/v1/info").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_method_the_route_does_not_serve_is_left_to_the_router() {
    let harness = Harness::new();
    let (status, _) = send_as(&harness, client(), &Method::DELETE, "/v1/info").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn health_and_metrics_need_no_principal() {
    let harness = Harness::new();
    for path in ["/healthz", "/metrics"] {
        let (status, body) = send_as(&harness, client(), &Method::GET, path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
}

#[tokio::test]
async fn a_route_added_outside_the_registry_is_refused() {
    let harness = Harness::new();
    let router: Router = Router::<Arc<Server>>::new()
        .route(
            "/v1/unregistered",
            axum::routing::get(|| async { "served" }),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&harness.server),
            authorization::authorize_operation,
        ))
        .with_state(Arc::clone(&harness.server));
    let request = Request::builder()
        .uri("/v1/unregistered")
        .body(Body::empty())
        .expect("request");
    let response = router.oneshot(request).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let error: Value = serde_json::from_slice(&bytes).expect("error body");
    assert_eq!(error["error"]["code"], "unregistered_operation");
}

#[tokio::test]
async fn a_token_request_is_served_as_the_owner() {
    let harness = Harness::with(HarnessOptions {
        token: "secret".to_string(),
        ..HarnessOptions::default()
    });
    let reply = harness
        .send_with(
            "GET",
            "/v1/info",
            None,
            &[("authorization", b"Bearer secret")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(harness.logged().contains("principal=owner"));
}

#[tokio::test]
async fn a_server_without_a_token_serves_requests_as_the_owner() {
    let harness = Harness::new();
    let reply = harness.send("GET", "/v1/info", None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(harness.logged().contains("principal=owner"));
}

#[tokio::test]
async fn the_socket_router_serves_requests_as_the_owner() {
    let harness = Harness::with(HarnessOptions {
        token: "secret".to_string(),
        ..HarnessOptions::default()
    });
    let reply = harness.send_on_socket("/v1/info").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(harness.logged().contains("principal=owner"));
}
