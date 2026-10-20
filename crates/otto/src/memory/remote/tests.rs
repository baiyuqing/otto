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

#[test]
fn wire_context_accepts_spaced_organization_only() {
    let mut context = wire::Context {
        organization_id: "bai's org".into(),
        user_id: "alice".into(),
        workspace_id: None,
        session_id: None,
    };
    assert!(validate_wire_context(&context).is_ok());
    context.user_id = "bad user".into();
    assert!(validate_wire_context(&context).is_err());
    context.user_id = "alice".into();
    context.organization_id = "bad\norg".into();
    assert!(validate_wire_context(&context).is_err());
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

mod mutations;
mod recall;
mod responses;
mod wire_decoding;
