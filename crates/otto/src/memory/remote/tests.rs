use super::*;
use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, Uri},
    response::Response,
};
use serde_json::{Value, json};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct Reply {
    status: u16,
    protocol: Option<&'static str>,
    body: Vec<u8>,
    delay: Duration,
}
fn reply(status: u16, body: Value) -> Reply {
    Reply {
        status,
        protocol: Some("1.0"),
        body: body.to_string().into_bytes(),
        delay: Duration::ZERO,
    }
}
type Seen = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;
async fn serve(answer: impl Fn(&str) -> Reply + Send + Sync + 'static) -> (String, Seen) {
    serve_request(move |path, _| answer(path)).await
}
async fn serve_request(
    answer: impl Fn(&str, &[u8]) -> Reply + Send + Sync + 'static,
) -> (String, Seen) {
    let seen: Seen = Seen::default();
    let log = seen.clone();
    let answer = Arc::new(answer);
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
        let (log, answer) = (log.clone(), answer.clone());
        async move {
            log.lock()
                .unwrap()
                .push((uri.path().to_string(), headers, body.to_vec()));
            let reply = answer(uri.path(), &body);
            tokio::time::sleep(reply.delay).await;
            let mut builder = Response::builder().status(reply.status);
            if let Some(protocol) = reply.protocol {
                builder = builder.header("Otto-Memory-Protocol", protocol);
            }
            builder.body(axum::body::Body::from(reply.body)).unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}
fn service(url: &str) -> RemoteService {
    RemoteService::new(Config {
        url: url.into(),
        token: "fixture-token".into(),
        recall_timeout: Duration::from_secs(3),
        logger: Arc::new(Logger::discard()),
    })
    .unwrap()
}
fn scope(namespace: &str) -> Scope {
    Scope {
        namespace: namespace.into(),
        id: match namespace {
            "organization" => "acme",
            "workspace" => "project-a",
            _ => "alice",
        }
        .into(),
    }
}
fn context(origin: Origin) -> MemoryContext {
    MemoryContext {
        organization_id: Some("acme".into()),
        principal: "alice".into(),
        scopes: vec![scope("organization"), scope("workspace"), scope("user")],
        write_scope: Some(scope("workspace")),
        workspace_id: Some("project-a".into()),
        session_id: Some("s-1".into()),
        task_id: None,
        origin,
    }
}
fn client(service: RemoteService) -> crate::memory::client::Client {
    crate::memory::client::Client::with_learning(
        Arc::new(service),
        LearningGrant {
            scopes: vec![scope("user"), scope("workspace")],
            input_classes: vec!["explicit_memory".into()],
            background: false,
        },
    )
    .unwrap()
}
fn recall_request() -> RecallRequest {
    RecallRequest {
        scope_filter: None,
        context: context(Origin::Model),
        query: "editor".into(),
        limit: 12,
        token_budget: 8192,
    }
}
fn item(id: &str, namespace: &str, key: Option<&str>, mandatory: bool) -> Value {
    json!({"id":id,"scope":scope(namespace),"revision":"r4","key":key,"text":"Use <vim> & tabs.","pinned":false,"mandatory":mandatory})
}
fn complete(items: Vec<Value>) -> Value {
    json!({"context":RemoteService::context(&context(Origin::Model)).unwrap(),"resolution":"workspace_organization_user","coverage":"complete","items":items})
}
async fn recall_error(body: Value) -> ErrorKind {
    let (url, _) = serve(move |_| reply(200, body.clone())).await;
    client(service(&url))
        .recall(&recall_request(), &CancellationToken::new())
        .await
        .unwrap_err()
        .kind
}
fn reference(namespace: &str) -> MemoryRef {
    MemoryRef {
        organization_id: Some("acme".into()),
        service: "remote".into(),
        scope: scope(namespace),
        id: "m-17".into(),
        revision: Some("r4".into()),
    }
}
fn remember_request() -> RememberRequest {
    RememberRequest {
        key: Some("editor.default".into()),
        context: context(Origin::Model),
        request_id: "0123456789abcdef0123456789abcdef".into(),
        content: Content::Text("Use vim.".into()),
        target: None,
        metadata: Default::default(),
    }
}
fn receipt(state: &str, namespace: &str, forget: bool) -> Value {
    json!({"request_id":if forget {"fedcba9876543210fedcba9876543210"} else {"0123456789abcdef0123456789abcdef"},"state":state,"item":if matches!(state,"applied"|"no_change") { json!({"organization_id":"acme","scope":scope(namespace),"id":"m-17","revision":"r5"}) } else { Value::Null }})
}
fn forget_request() -> ForgetRequest {
    ForgetRequest {
        context: context(Origin::Model),
        request_id: "fedcba9876543210fedcba9876543210".into(),
        target: ForgetTarget::Item(reference("user")),
        coverage: ForgetLevel::Suppress,
    }
}

#[tokio::test]
async fn recall_sends_fixed_context_and_preserves_explicit_projection() {
    let (url, seen) = serve(|_| {
        reply(
            200,
            complete(vec![
                item("m-p", "workspace", Some("language"), true),
                item("m-o", "organization", Some("editor"), false),
                item("m-u", "user", None, false),
            ]),
        )
    })
    .await;
    let result = client(service(&url))
        .recall(&recall_request(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.items.len(), 3);
    let projection = result.items[0].remote.as_ref().unwrap();
    assert!(projection.mandatory && !projection.pinned);
    assert_eq!(projection.view, MemoryView::Effective);
    assert_eq!(
        result.items[0]
            .reference
            .as_ref()
            .unwrap()
            .organization_id
            .as_deref(),
        Some("acme")
    );
    let seen = seen.lock().unwrap();
    let (_, headers, body) = &seen[0];
    assert_eq!(headers["authorization"], "Bearer fixture-token");
    assert_eq!(headers["otto-memory-protocol"], "1.0");
    let body: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        body["context"],
        json!({"organization_id":"acme","user_id":"alice","workspace_id":"project-a","session_id":"s-1"})
    );
    assert!(body["scope_filter"].is_null());
    assert!(body["context"].get("scopes").is_none());
}
#[tokio::test]
async fn recall_discards_every_invalid_response_without_partial_results() {
    let valid = complete(vec![
        item("m-p", "workspace", Some("language"), true),
        item("m-o", "organization", Some("editor"), false),
    ]);
    let mut cases = Vec::new();
    for coverage in ["partial", "unknown"] {
        let mut v = valid.clone();
        v["coverage"] = json!(coverage);
        cases.push(v);
    }
    for field in ["organization_id", "user_id", "workspace_id", "session_id"] {
        let mut v = valid.clone();
        v["context"][field] = json!("foreign");
        cases.push(v);
    }
    for field in ["revision", "mandatory"] {
        let mut v = valid.clone();
        v["items"][0].as_object_mut().unwrap().remove(field);
        cases.push(v);
    }
    for (field, value) in [
        ("revision", json!("")),
        ("key", json!("Bad Key")),
        ("text", json!("")),
        ("id", json!("has space")),
        ("mandatory", json!("yes")),
    ] {
        let mut v = valid.clone();
        v["items"][0][field] = value;
        cases.push(v);
    }
    let mut v = valid.clone();
    v["items"][0]["scope"]["namespace"] = json!("global");
    cases.push(v);
    let mut v = valid.clone();
    v["items"][0]["scope"]["id"] = json!("foreign");
    cases.push(v);
    let mut v = valid.clone();
    v["resolution"] = json!("user_first");
    cases.push(v);
    cases.push(complete(vec![
        item("same", "user", None, false),
        item("same", "user", None, false),
    ]));
    cases.push(complete(vec![
        item("a", "workspace", Some("same"), false),
        item("b", "user", Some("same"), false),
    ]));
    cases.push(complete(vec![
        item("a", "user", None, false),
        item("b", "workspace", None, false),
    ]));
    cases.push(complete(vec![
        item("a", "workspace", None, false),
        item("b", "user", None, true),
    ]));
    let mut v = complete(vec![item("a", "user", None, false)]);
    v["items"][0]["pinned"] = json!(true);
    cases.push(v);
    for (i, body) in cases.into_iter().enumerate() {
        assert_eq!(recall_error(body).await, ErrorKind::Unavailable, "case {i}");
    }
    assert_eq!(
        recall_error(json!({"coverage":"complete","items":[]})).await,
        ErrorKind::Unavailable,
        "old v1 body"
    );
}
#[tokio::test]
async fn scoped_search_is_filtered_after_resolution_contract() {
    let (url, seen) = serve(|_| {
        reply(
            200,
            complete(vec![item("a", "organization", Some("language"), false)]),
        )
    })
    .await;
    let client = client(service(&url));
    let mut request = recall_request();
    request.scope_filter = Some(vec![ScopeNamespace::Organization]);
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert_eq!(body["scope_filter"], json!(["organization"]));
    request.scope_filter = Some(vec![ScopeNamespace::User]);
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unavailable
    );
    request.scope_filter = Some(vec![]);
    assert!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn golden_rendering_and_exact_escaped_byte_budget() {
    let fixture = include_str!("fixtures/v1/recall_response.json");
    let expected = include_str!("fixtures/v1/recall_rendered.txt");
    let response: wire::RecallResponse = wire::decode(fixture.as_bytes()).unwrap();
    let records: Vec<_> = response
        .items
        .into_iter()
        .map(|r| {
            record(
                &RemoteService::item(
                    r.item,
                    &context(Origin::Model),
                    r.mandatory,
                    MemoryView::Effective,
                )
                .unwrap(),
            )
        })
        .collect();
    assert_eq!(render_memory_context(&records), expected);
    let (url, _) = serve(move |_| Reply {
        status: 200,
        protocol: Some("1.0"),
        body: fixture.as_bytes().to_vec(),
        delay: Duration::ZERO,
    })
    .await;
    let client = client(service(&url));
    let mut request = recall_request();
    request.token_budget = expected.len();
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap()
            .used_tokens,
        Some(expected.len())
    );
    request.token_budget -= 1;
    assert_eq!(
        client
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unavailable
    );
    assert_eq!(render_memory_context(&[]), "");
}
#[tokio::test]
async fn strict_status_protocol_duplicates_size_timeout_and_cancellation() {
    for status in [201, 202, 204, 206] {
        let (url, _) = serve(move |_| reply(status, complete(vec![]))).await;
        assert_eq!(
            client(service(&url))
                .recall(&recall_request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
    }
    for protocol in [None, Some("2.0"), Some("1."), Some("x1.0")] {
        let (url, _) = serve(move |_| {
            let mut r = reply(200, complete(vec![]));
            r.protocol = protocol;
            r
        })
        .await;
        assert!(
            client(service(&url))
                .recall(&recall_request(), &CancellationToken::new())
                .await
                .is_err()
        );
    }
    let (url, _) = serve(|_| {
        let mut r = reply(200, complete(vec![]));
        r.protocol = Some("1.23");
        r
    })
    .await;
    assert!(
        client(service(&url))
            .recall(&recall_request(), &CancellationToken::new())
            .await
            .is_ok()
    );
    for body in [
        b"{\"coverage\":\"complete\",\"coverage\":\"partial\"}".to_vec(),
        vec![b' '; MAX_RESPONSE_BYTES + 1],
    ] {
        let (url, _) = serve(move |_| Reply {
            status: 200,
            protocol: Some("1.0"),
            body: body.clone(),
            delay: Duration::ZERO,
        })
        .await;
        assert!(
            client(service(&url))
                .recall(&recall_request(), &CancellationToken::new())
                .await
                .is_err()
        );
    }
    let (url, _) = serve(|_| {
        let mut r = reply(200, complete(vec![]));
        r.delay = Duration::from_millis(150);
        r
    })
    .await;
    let mut backend = service(&url);
    backend.recall_timeout = Duration::from_millis(10);
    assert_eq!(
        backend
            .recall(&recall_request(), &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unavailable
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        service(&url)
            .recall(&recall_request(), &cancel)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Canceled
    );
}
#[tokio::test]
async fn raw_get_checks_tenant_scope_and_status_and_never_returns_effective_view() {
    let (url,seen)=serve(|_|reply(200,json!({"view":"raw","organization_id":"acme","item":{"id":"m-17","scope":scope("user"),"revision":"r4","key":"editor","text":"shadowed","pinned":false}}))).await;
    let client = client(service(&url));
    let request = InspectRequest {
        context: context(Origin::Human),
        query: InspectQuery::Get(reference("user")),
    };
    let result = client
        .inspect(&request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        result.items[0].remote.as_ref().unwrap().view,
        MemoryView::Raw
    );
    assert_eq!(result.state, "raw");
    assert_eq!(seen.lock().unwrap().len(), 1);
    for body in [
        json!({"view":"effective","organization_id":"acme","item":{}}),
        json!({"view":"raw","organization_id":"other","item":{}}),
    ] {
        let (url, _) = serve(move |_| reply(200, body.clone())).await;
        assert!(
            service(&url)
                .inspect(&request, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    for (status, body, ok) in [
        (404, json!({}), true),
        (404, json!({"error":"not found"}), false),
        (201, json!({}), false),
    ] {
        let (url, _) = serve(move |_| reply(status, body.clone())).await;
        assert_eq!(
            service(&url)
                .inspect(&request, &CancellationToken::new())
                .await
                .is_ok(),
            ok
        );
    }
}
#[tokio::test]
async fn references_and_model_organization_mutations_fail_before_dispatch() {
    let (url, seen) = serve(|_| reply(200, json!({}))).await;
    let backend = client(service(&url));
    for organization in [None, Some("foreign".into())] {
        let mut target = reference("user");
        target.organization_id = organization;
        let request = InspectRequest {
            context: context(Origin::Model),
            query: InspectQuery::Get(target),
        };
        assert!(
            backend
                .inspect(&request, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    let mut remember = remember_request();
    remember.context.write_scope = Some(scope("organization"));
    assert!(
        backend
            .remember(&remember, &CancellationToken::new())
            .await
            .is_err()
    );
    let mut forget = forget_request();
    forget.target = ForgetTarget::Item(reference("organization"));
    assert!(
        backend
            .forget(&forget, &CancellationToken::new())
            .await
            .is_err()
    );
    let mut update = remember_request();
    let mut target = reference("workspace");
    target.revision = None;
    update.target = Some(target);
    assert!(
        backend
            .remember(&update, &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn remember_and_forget_receipts_validate_all_states_and_request_identity() {
    for state in [
        "applied",
        "no_change",
        "skipped",
        "failed",
        "pending",
        "unknown",
    ] {
        let (url, seen) = serve(move |_| {
            reply(
                if state == "pending" { 202 } else { 200 },
                receipt(state, "workspace", false),
            )
        })
        .await;
        let result = client(service(&url))
            .remember(&remember_request(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.items[0].reference.is_some(),
            matches!(state, "applied" | "no_change")
        );
        if matches!(state, "pending" | "unknown") {
            assert_ne!(result.completion, Completion::Complete);
        }
        let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
        assert_eq!(body["key"], "editor.default");
        assert!(body["context"].get("scopes").is_none());
        let (url, _) = serve(move |_| {
            reply(
                if state == "pending" { 202 } else { 200 },
                receipt(state, "user", true),
            )
        })
        .await;
        let result = client(service(&url))
            .forget(&forget_request(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.items[0].reference.is_some(),
            matches!(state, "applied" | "no_change")
        );
    }
    let valid = receipt("applied", "workspace", false);
    let mut cases = Vec::new();
    for (field, value) in [
        ("organization_id", json!("foreign")),
        ("revision", json!("")),
        ("id", json!("")),
    ] {
        let mut v = valid.clone();
        v["item"][field] = value;
        cases.push(v);
    }
    let mut v = valid.clone();
    v["request_id"] = json!("wrong");
    cases.push(v);
    let mut v = valid.clone();
    v["item"]["scope"] = json!(scope("user"));
    cases.push(v);
    let mut v = valid.clone();
    v["state"] = json!("skipped");
    cases.push(v);
    cases.push(json!({"state":"applied","id":"m-17"}));
    for body in cases {
        let (url, _) = serve(move |_| reply(200, body.clone())).await;
        assert_eq!(
            client(service(&url))
                .remember(&remember_request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::OutcomeUnknown
        );
    }
    for (status, state) in [(200, "pending"), (202, "applied"), (201, "applied")] {
        let (url, _) = serve(move |_| reply(status, receipt(state, "workspace", false))).await;
        assert_eq!(
            client(service(&url))
                .remember(&remember_request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::OutcomeUnknown
        );
    }
}
#[tokio::test]
async fn human_organization_write_preserves_full_context_and_service_acl() {
    let (url, seen) = serve(|_| reply(200, receipt("applied", "organization", false))).await;
    let mut request = remember_request();
    request.context.origin = Origin::Human;
    request.context.write_scope = Some(scope("organization"));
    assert_eq!(
        client(service(&url))
            .remember(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
    assert!(seen.lock().unwrap().is_empty());
    let result = client(service(&url))
        .remember_organization_human(&request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.completion, Completion::Complete);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert_eq!(sent["origin"], "human");
    assert_eq!(sent["scope"], json!(scope("organization")));
    assert_eq!(sent["context"]["workspace_id"], "project-a");
    let (url, _) = serve(|_| {
        reply(
            403,
            json!({"error":{"code":"forbidden","message":"untrusted diagnostic"}}),
        )
    })
    .await;
    assert_eq!(
        client(service(&url))
            .remember_organization_human(&request, &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Unauthorized
    );
}
#[tokio::test]
async fn errors_are_fixed_and_write_timeout_is_unknown() {
    for (status, code, expected) in [
        (400, "invalid_input", ErrorKind::InvalidInput),
        (401, "unauthorized", ErrorKind::Unauthorized),
        (403, "forbidden", ErrorKind::Unauthorized),
        (409, "conflict", ErrorKind::Conflict),
        (409, "idempotency_conflict", ErrorKind::Conflict),
        (413, "too_large", ErrorKind::InvalidInput),
        (429, "rate_limited", ErrorKind::RateLimited),
        (503, "unavailable", ErrorKind::Unavailable),
        (400, "unsupported_version", ErrorKind::Unsupported),
        (401, "bogus", ErrorKind::Unavailable),
    ] {
        let (url, _) = serve(move |_| {
            reply(
                status,
                json!({"error":{"code":code,"message":"do not expose"}}),
            )
        })
        .await;
        let error = service(&url)
            .recall(&recall_request(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, expected);
        assert!(!error.to_string().contains("do not expose"));
    }
    let (url, _) = serve(|_| {
        let mut r = reply(200, receipt("applied", "workspace", false));
        r.delay = Duration::from_millis(150);
        r
    })
    .await;
    let mut backend = service(&url);
    backend.write_timeout = Duration::from_millis(10);
    assert_eq!(
        backend
            .remember(&remember_request(), &CancellationToken::new())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::OutcomeUnknown
    );
}
#[tokio::test]
async fn turns_require_valid_context_and_exact_202_empty_object() {
    let turn = wire::TurnRequest {
        context: RemoteService::context(&context(Origin::Model)).unwrap(),
        turn_id: "0123456789abcdef0123456789abcdef".into(),
        status: wire::TurnStatus::Success,
        messages: vec![],
    };
    for (status, body, ok) in [
        (202, json!({}), true),
        (200, json!({}), false),
        (202, json!({"ok":true}), false),
    ] {
        let (url, _) = serve(move |_| reply(status, body.clone())).await;
        assert_eq!(service(&url).upload_turn(&turn).await.is_ok(), ok);
    }
}
#[test]
fn wire_rejects_nested_duplicate_unknown_fields_and_unknown_enums() {
    assert!(wire::decode::<Value>(br#"{"extra":{"same":1,"same":2}}"#).is_err());
    assert!(wire::decode::<Value>(br#"[{"same":1,"same":2}]"#).is_err());
    assert!(wire::decode::<wire::Scope>(br#"{"namespace":"global","id":"a"}"#).is_err());
    assert!(wire::decode::<Value>(br#"{"extra":[null,true,3,1.5,"x"]}"#).is_ok());
}
#[test]
fn constructor_rejects_non_loopback_http_and_redirecting_urls_without_network() {
    for url in [
        "http://example.com",
        "https://u:p@example.com",
        "https://example.com/v1",
        "https://example.com?q=1",
        "https://example.com#fragment",
    ] {
        assert!(
            RemoteService::new(Config {
                url: url.into(),
                token: "fixture-token".into(),
                recall_timeout: Duration::from_secs(3),
                logger: Arc::new(Logger::discard())
            })
            .is_err(),
            "{url}"
        );
    }
}

#[test]
fn every_v1_fixture_round_trips_and_missing_nullable_item_key_is_invalid() {
    fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(fixture: &str) {
        let value: T = wire::decode(fixture.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::from_str::<Value>(fixture).unwrap()
        );
    }
    round_trip::<wire::RecallBody>(include_str!("fixtures/v1/recall_request.json"));
    round_trip::<wire::RecallResponse>(include_str!("fixtures/v1/recall_response.json"));
    round_trip::<wire::GetBody>(include_str!("fixtures/v1/get_request.json"));
    round_trip::<wire::GetResponse>(include_str!("fixtures/v1/get_response.json"));
    round_trip::<wire::RememberBody>(include_str!("fixtures/v1/remember_request.json"));
    round_trip::<wire::ForgetBody>(include_str!("fixtures/v1/forget_request.json"));
    round_trip::<wire::Receipt>(include_str!("fixtures/v1/receipt.json"));
    round_trip::<wire::TurnRequest>(include_str!("fixtures/v1/turns_request.json"));
    round_trip::<wire::ErrorBody>(include_str!("fixtures/v1/error.json"));
    let mut recalled = item("a", "user", None, false);
    recalled.as_object_mut().unwrap().remove("key");
    assert!(wire::decode::<wire::RecalledItem>(&serde_json::to_vec(&recalled).unwrap()).is_err());
}
#[tokio::test]
async fn no_project_context_has_only_organization_and_user_and_defaults_to_user() {
    let (url, seen) = serve(|_| reply(200, receipt("applied", "user", false))).await;
    let mut request = remember_request();
    request.context.workspace_id = None;
    request
        .context
        .scopes
        .retain(|scope| scope.namespace != "workspace");
    request.context.write_scope = Some(scope("user"));
    client(service(&url))
        .remember(&request, &CancellationToken::new())
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert!(body["context"]["workspace_id"].is_null());
    assert_eq!(body["scope"], json!(scope("user")));
}
#[tokio::test]
async fn model_tools_bind_references_and_nullable_key_without_tenant_selection() {
    use crate::tool::memory_service::tools;
    use serde_json::value::RawValue;
    let (url, seen) = serve_request(|path, body| {
        let request: Value = serde_json::from_slice(body).unwrap();
        match path {
            "/v1/remember" | "/v1/forget" => {
                let mut result = receipt(
                    "applied",
                    if path == "/v1/remember" {
                        "workspace"
                    } else {
                        "user"
                    },
                    path == "/v1/forget",
                );
                result["request_id"] = request["request_id"].clone();
                reply(200, result)
            }
            _ => reply(200, complete(vec![])),
        }
    })
    .await;
    let tools = tools(
        Arc::new(client(service(&url))),
        context(Origin::Model),
        8192,
    );
    let remember = tools
        .iter()
        .find(|tool| tool.definition().name == "remember")
        .unwrap();
    let forget = tools
        .iter()
        .find(|tool| tool.definition().name == "forget")
        .unwrap();
    let search = tools
        .iter()
        .find(|tool| tool.definition().name == "memory_search")
        .unwrap();
    let call = |value: Value| RawValue::from_string(value.to_string()).unwrap();
    for args in [
        json!({"text":"create","organization_id":"other"}),
        json!({"text":"org","scope":"organization"}),
        json!({"text":"update","target_id":"m-17","scope_namespace":"workspace","scope_id":"project-a","opaque_revision":"r4"}),
        json!({"text":"update","target_id":"m-17","organization_id":"acme","scope_namespace":"workspace","scope_id":"project-a"}),
        json!({"text":"update","target_id":"m-17","organization_id":"other","scope_namespace":"workspace","scope_id":"project-a","opaque_revision":"r4"}),
    ] {
        assert!(
            remember
                .execute(&call(args), &CancellationToken::new())
                .await
                .is_error
        );
    }
    assert!(forget.execute(&call(json!({"id":"m-17","organization_id":"acme","scope_namespace":"organization","scope_id":"acme","opaque_revision":"r4"})),&CancellationToken::new()).await.is_error);
    assert!(search.execute(&call(json!({"id":"m-17","organization_id":"other","scope_namespace":"user","scope_id":"alice","opaque_revision":"r4"})),&CancellationToken::new()).await.is_error);
    assert!(seen.lock().unwrap().is_empty());
    let result=remember.execute(&call(json!({"text":"new value","key":"editor.default","target_id":"m-17","organization_id":"acme","scope_namespace":"workspace","scope_id":"project-a","opaque_revision":"r4"})),&CancellationToken::new()).await;
    assert!(!result.is_error, "{}", result.content);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
    assert_eq!(sent["key"], "editor.default");
    assert_eq!(sent["target"], json!({"id":"m-17","revision":"r4"}));
    assert_eq!(sent["context"]["organization_id"], "acme");
    assert!(sent["metadata"].get("key").is_none());
    let result = remember
        .execute(
            &call(json!({"text":"unkeyed","key":null})),
            &CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[1].2).unwrap();
    assert!(sent["key"].is_null());
}
