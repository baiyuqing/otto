//! HTTP client for a remote memory service speaking protocol 1.x
//! (`docs/specs/2026-10-17-remote-memory-protocol.md`). The service is
//! untrusted: every response is checked before use and a violation discards
//! the whole result. Port errors carry only fixed text; server messages go to
//! the log only.

pub mod upload;
pub mod wire;

use std::{sync::Arc, time::Duration};

use otto_core::{
    agent::memory::{self as agent, Record, render_memory_context},
    memory::*,
    operation::OperationControl,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::log::Logger;

const IDENTITY: &str = "remote";
const PROTOCOL_REQUEST: &str = "1.0";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TURN_BYTES: usize = 4 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LOGGED_MESSAGE_BYTES: usize = 512;

pub struct Config {
    /// Base URL without a trailing slash.
    pub url: String,
    /// Bearer token. Never logged or formatted.
    pub token: String,
    /// Applies to `/v1/recall` and `/v1/get`.
    pub recall_timeout: Duration,
    pub logger: Arc<Logger>,
}

pub struct RemoteService {
    http: reqwest::Client,
    base: String,
    token: String,
    recall_timeout: Duration,
    write_timeout: Duration,
    logger: Arc<Logger>,
}

/// A response whose protocol header and size were accepted.
struct Raw {
    status: u16,
    body: Vec<u8>,
}

fn err(kind: ErrorKind) -> Error {
    Error::new(kind)
}

impl RemoteService {
    pub fn new(config: Config) -> std::result::Result<Self, String> {
        let url = reqwest::Url::parse(&config.url).map_err(|_| "invalid memory service url")?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("invalid memory service url".into());
        }
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.token))
            .map_err(|_| "invalid memory service token")?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "cannot build memory service client")?;
        Ok(Self {
            http,
            base: config.url.trim_end_matches('/').to_string(),
            token: config.token,
            recall_timeout: config.recall_timeout,
            write_timeout: WRITE_TIMEOUT,
            logger: config.logger,
        })
    }

    /// POST /v1/turns, 30 s timeout. Any 2xx is success.
    pub async fn upload_turn(&self, turn: &wire::TurnRequest) -> Result<()> {
        let body = serde_json::to_vec(turn).map_err(|_| err(ErrorKind::InvalidInput))?;
        if body.len() > MAX_TURN_BYTES {
            return Err(err(ErrorKind::InvalidInput));
        }
        self.exchange(
            "turns",
            body,
            TURN_TIMEOUT,
            false,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?
        .into_success(self, "turns")
        .map(|_| ())
    }

    fn warn(&self, endpoint: &str, status: u16, code: &str, message: &str) {
        self.logger.warn(
            "memory_request_failed",
            &[
                ("endpoint", endpoint.to_string()),
                ("status", status.to_string()),
                ("code", code.to_string()),
                (
                    "message",
                    truncate(message, MAX_LOGGED_MESSAGE_BYTES).to_string(),
                ),
            ],
        );
    }

    /// Sends one request. `write` selects OutcomeUnknown for failures after
    /// the request may have reached the service.
    async fn exchange(
        &self,
        endpoint: &'static str,
        body: Vec<u8>,
        timeout: Duration,
        write: bool,
        control: &(impl OperationControl + ?Sized),
    ) -> Result<Raw> {
        let after_send = if write {
            ErrorKind::OutcomeUnknown
        } else {
            ErrorKind::Unavailable
        };
        let request = async {
            let mut response = self
                .http
                .post(format!("{}/v1/{endpoint}", self.base))
                .bearer_auth(&self.token)
                .header("Otto-Memory-Protocol", PROTOCOL_REQUEST)
                .header(
                    reqwest::header::USER_AGENT,
                    concat!("otto/", env!("CARGO_PKG_VERSION")),
                )
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/json; charset=utf-8",
                )
                .timeout(timeout)
                .body(body)
                .send()
                .await
                .map_err(|error| {
                    self.warn(endpoint, 0, "transport", "request failed");
                    if error.is_connect() {
                        err(ErrorKind::Unavailable)
                    } else {
                        err(after_send)
                    }
                })?;
            let status = response.status().as_u16();
            let accepted = response
                .headers()
                .get("Otto-Memory-Protocol")
                .and_then(|value| value.to_str().ok())
                .is_some_and(supported_protocol);
            if !accepted {
                self.warn(
                    endpoint,
                    status,
                    "invalid_response",
                    "missing or unsupported protocol header",
                );
                return Err(err(after_send));
            }
            let mut bytes = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        bytes.extend_from_slice(&chunk);
                        if bytes.len() > MAX_RESPONSE_BYTES {
                            self.warn(
                                endpoint,
                                status,
                                "invalid_response",
                                "response body over 1 MiB",
                            );
                            return Err(err(after_send));
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        self.warn(endpoint, status, "transport", "response body read failed");
                        return Err(err(after_send));
                    }
                }
            }
            Ok(Raw {
                status,
                body: bytes,
            })
        };
        tokio::select! {
            _ = control.cancellation_token().cancelled() => Err(err(ErrorKind::Canceled)),
            raw = request => raw,
        }
    }

    /// Exchanges one JSON request and decodes a 2xx body.
    async fn call<B: Serialize, R: DeserializeOwned>(
        &self,
        endpoint: &'static str,
        body: &B,
        timeout: Duration,
        write: bool,
        control: &dyn OperationControl,
    ) -> Result<R> {
        let body = serde_json::to_vec(body).map_err(|_| err(ErrorKind::InvalidInput))?;
        let raw = self
            .exchange(endpoint, body, timeout, write, control)
            .await?;
        let raw = raw.into_success(self, endpoint)?;
        serde_json::from_slice(&raw.body).map_err(|_| {
            self.warn(
                endpoint,
                raw.status,
                "invalid_response",
                "body does not decode",
            );
            err(if write {
                ErrorKind::OutcomeUnknown
            } else {
                ErrorKind::Unavailable
            })
        })
    }

    /// Converts a wire item to a port item. `None` when any rule is violated.
    fn item(item: wire::Item, scopes: &[Scope]) -> Option<RecallItem> {
        if !scopes.contains(&item.scope)
            || !valid_opaque_id(&item.id, MAX_ID_BYTES)
            || item
                .revision
                .as_deref()
                .is_some_and(|revision| !valid_opaque_id(revision, MAX_ID_BYTES))
            || Content::Text(item.text.clone()).validate().is_err()
        {
            return None;
        }
        Some(RecallItem {
            reference: Some(MemoryRef {
                service: IDENTITY.into(),
                scope: item.scope,
                id: item.id,
                revision: item.revision,
            }),
            content: Content::Text(item.text),
            sources: Vec::new(),
            effective_at: None,
            metadata: Default::default(),
        })
    }

    fn context(context: &MemoryContext) -> wire::Context {
        wire::Context {
            scopes: context.scopes.clone(),
            session_id: context.session_id.clone(),
        }
    }

    fn origin(context: &MemoryContext) -> Result<wire::Origin> {
        match context.origin {
            Origin::Human => Ok(wire::Origin::Human),
            Origin::Model => Ok(wire::Origin::Model),
            Origin::Extractor => Err(err(ErrorKind::InvalidInput)),
        }
    }

    /// Maps a wire receipt to a port receipt. `forget` selects the forget
    /// coverage the port requires for a completed forget.
    fn receipt(
        &self,
        endpoint: &str,
        request_id: &str,
        scope: &Scope,
        wire: wire::Receipt,
        forget: bool,
    ) -> Result<Receipt> {
        use wire::ReceiptState::*;
        let invalid = || {
            self.warn(endpoint, 200, "invalid_response", "invalid receipt");
            err(ErrorKind::OutcomeUnknown)
        };
        let reference = match wire.id {
            Some(id)
                if valid_opaque_id(&id, MAX_ID_BYTES)
                    && wire
                        .revision
                        .as_deref()
                        .is_none_or(|revision| valid_opaque_id(revision, MAX_ID_BYTES)) =>
            {
                Some(MemoryRef {
                    service: IDENTITY.into(),
                    scope: scope.clone(),
                    id,
                    revision: wire.revision,
                })
            }
            Some(_) => return Err(invalid()),
            None if matches!(wire.state, Applied | NoChange) => return Err(invalid()),
            None => None,
        };
        let (admission, completion, visibility, state) = match wire.state {
            Applied => (
                Admission::Acknowledged,
                Completion::Complete,
                Visibility::Ready,
                ItemState::Applied,
            ),
            NoChange => (
                Admission::Acknowledged,
                Completion::Complete,
                Visibility::Ready,
                ItemState::NoChange,
            ),
            // A completed forget must report the achieved level, which a
            // declined forget did not reach.
            Skipped => (
                Admission::Acknowledged,
                if forget {
                    Completion::Partial
                } else {
                    Completion::Complete
                },
                Visibility::NotApplicable,
                ItemState::Skipped,
            ),
            Pending => (
                Admission::Acknowledged,
                Completion::Pending,
                Visibility::Pending,
                ItemState::Unknown,
            ),
            Failed => (
                Admission::Rejected,
                Completion::Failed,
                Visibility::NotApplicable,
                ItemState::Failed,
            ),
        };
        let forget_coverage = forget.then(|| {
            if matches!(wire.state, Applied | NoChange) {
                ForgetCoverage {
                    achieved: vec![ForgetLevel::Suppress],
                    remaining: Vec::new(),
                }
            } else {
                ForgetCoverage {
                    achieved: Vec::new(),
                    remaining: vec![ForgetLevel::Suppress],
                }
            }
        });
        Ok(Receipt {
            request_id: request_id.into(),
            operation_ref: None,
            admission,
            completion,
            visibility,
            items: vec![ItemOutcome {
                reference,
                state,
                reason: (state == ItemState::Failed).then_some(ErrorKind::Rejected),
            }],
            forget_coverage,
        })
    }
}

impl Raw {
    /// Returns the response for a 2xx status; otherwise logs and maps the
    /// error body.
    fn into_success(self, service: &RemoteService, endpoint: &str) -> Result<Raw> {
        if (200..300).contains(&self.status) {
            return Ok(self);
        }
        let parsed = serde_json::from_slice::<wire::ErrorBody>(&self.body).ok();
        let (code, message) = parsed
            .as_ref()
            .map_or(("", ""), |body| (&body.error.code, &body.error.message));
        service.warn(endpoint, self.status, code, message);
        Err(err(error_kind(self.status, code)))
    }
}

fn error_kind(status: u16, code: &str) -> ErrorKind {
    match (status, code) {
        (400, "invalid_input") | (413, "too_large") => ErrorKind::InvalidInput,
        (401, "unauthorized") | (403, "forbidden") => ErrorKind::Unauthorized,
        (409, "conflict") => ErrorKind::Conflict,
        (429, "rate_limited") => ErrorKind::RateLimited,
        (400, "unsupported_version") => ErrorKind::Unsupported,
        _ => ErrorKind::Unavailable,
    }
}

/// Accepts `1.<minor>`.
fn supported_protocol(value: &str) -> bool {
    value
        .strip_prefix("1.")
        .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()))
}

/// The record Otto renders for one recalled item; `ServiceBinding::recall`
/// in `runtime/wiring.rs` builds the same record from the port item.
fn record(item: &wire::Item) -> Record {
    Record {
        id: item.id.clone(),
        scope: agent::Scope {
            namespace: item.scope.namespace.clone(),
            id: item.scope.id.clone(),
        },
        kind: "reference".into(),
        key: String::new(),
        text: item.text.clone(),
        standing: false,
    }
}

fn truncate(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[async_trait::async_trait]
impl MemoryService for RemoteService {
    fn describe(&self) -> ServiceDescription {
        let capability = |operation, effects: Vec<Effect>| Capability {
            effects,
            supports_delegated_learning: true,
            ..Capability::new(operation)
        };
        let mut remember = capability(Operation::Remember, vec![Effect::ChangeMemory]);
        remember.conditional_updates = true;
        let mut forget = capability(Operation::Forget, vec![Effect::ChangeMemory]);
        forget.forget_targets = vec![ForgetTargetKind::Item];
        forget.forget_levels = vec![ForgetLevel::Suppress];
        ServiceDescription {
            identity: IDENTITY.into(),
            version: 1,
            capabilities: vec![
                capability(Operation::Recall, Vec::new()),
                capability(Operation::Inspect, Vec::new()),
                remember,
                forget,
            ],
            encryption_at_rest: None,
        }
    }

    async fn recall(
        &self,
        request: &RecallRequest,
        control: &dyn OperationControl,
    ) -> Result<RecallResult> {
        let body = wire::RecallBody {
            context: Self::context(&request.context),
            query: request.query.clone(),
            limit: request.limit,
            max_bytes: request.token_budget,
        };
        let response: wire::RecallResponse = self
            .call("recall", &body, self.recall_timeout, false, control)
            .await?;
        let rejected = |reason: &str| {
            self.warn("recall", 200, "invalid_response", reason);
            err(ErrorKind::Unavailable)
        };
        if response.coverage != wire::Coverage::Complete {
            return Err(rejected("coverage is not complete"));
        }
        if response.items.len() > request.limit {
            return Err(rejected("more items than limit"));
        }
        let mut items = Vec::with_capacity(response.items.len());
        let mut records = Vec::with_capacity(response.items.len());
        for item in response.items {
            let record = record(&item);
            let item = Self::item(item, &request.context.scopes)
                .ok_or_else(|| rejected("item violates the protocol rules"))?;
            records.push(record);
            items.push(item);
        }
        let used = render_memory_context(&records).len();
        if used > request.token_budget {
            return Err(rejected("rendering exceeds max_bytes"));
        }
        Ok(RecallResult {
            items,
            used_tokens: Some(used),
            coverage: Coverage::Complete,
        })
    }

    async fn inspect(
        &self,
        request: &InspectRequest,
        control: &dyn OperationControl,
    ) -> Result<InspectResult> {
        let InspectQuery::Get(reference) = &request.query else {
            return Err(err(ErrorKind::Unsupported));
        };
        let body = serde_json::to_vec(&wire::GetBody {
            context: Self::context(&request.context),
            scope: reference.scope.clone(),
            id: reference.id.clone(),
        })
        .map_err(|_| err(ErrorKind::InvalidInput))?;
        let raw = self
            .exchange("get", body, self.recall_timeout, false, control)
            .await?;
        let mut result = InspectResult {
            items: Vec::new(),
            next_cursor: String::new(),
            state: String::new(),
        };
        if raw.status == 404 {
            return Ok(result);
        }
        let raw = raw.into_success(self, "get")?;
        let invalid = || {
            self.warn("get", raw.status, "invalid_response", "invalid item");
            err(ErrorKind::Unavailable)
        };
        let response: wire::GetResponse =
            serde_json::from_slice(&raw.body).map_err(|_| invalid())?;
        if response.item.id != reference.id || response.item.scope != reference.scope {
            return Err(invalid());
        }
        result
            .items
            .push(Self::item(response.item, &request.context.scopes).ok_or_else(invalid)?);
        Ok(result)
    }

    async fn remember(
        &self,
        request: &RememberRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        let Content::Text(text) = &request.content else {
            return Err(err(ErrorKind::InvalidInput));
        };
        let scope = match &request.target {
            Some(target) => target.scope.clone(),
            None => request
                .context
                .write_scope
                .clone()
                .ok_or_else(|| err(ErrorKind::InvalidInput))?,
        };
        let body = wire::RememberBody {
            context: Self::context(&request.context),
            request_id: request.request_id.clone(),
            origin: Self::origin(&request.context)?,
            scope: scope.clone(),
            text: text.clone(),
            target: request.target.as_ref().map(|target| wire::Target {
                id: target.id.clone(),
                revision: target.revision.clone(),
            }),
            metadata: request.metadata.clone(),
        };
        let receipt: wire::Receipt = self
            .call("remember", &body, self.write_timeout, true, control)
            .await?;
        self.receipt("remember", &request.request_id, &scope, receipt, false)
    }

    async fn forget(
        &self,
        request: &ForgetRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        let ForgetTarget::Item(target) = &request.target else {
            return Err(err(ErrorKind::Unsupported));
        };
        if request.coverage != ForgetLevel::Suppress {
            return Err(err(ErrorKind::Unsupported));
        }
        let body = wire::ForgetBody {
            context: Self::context(&request.context),
            request_id: request.request_id.clone(),
            origin: Self::origin(&request.context)?,
            scope: target.scope.clone(),
            id: target.id.clone(),
            revision: target.revision.clone(),
        };
        let receipt: wire::Receipt = self
            .call("forget", &body, self.write_timeout, true, control)
            .await?;
        self.receipt("forget", &request.request_id, &target.scope, receipt, true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, Uri},
        response::Response,
    };
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::memory::client::Client;

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

    /// Starts a loopback stub that answers every request with `answer(path)`.
    async fn serve(answer: impl Fn(&str) -> Reply + Send + Sync + 'static) -> (String, Seen) {
        let seen: Seen = Seen::default();
        let log = Arc::clone(&seen);
        let answer = Arc::new(answer);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let (log, answer) = (Arc::clone(&log), Arc::clone(&answer));
            async move {
                log.lock()
                    .unwrap()
                    .push((uri.path().to_string(), headers, body.to_vec()));
                let reply = answer(uri.path());
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

    fn service(url: &str, recall_timeout: Duration) -> RemoteService {
        RemoteService::new(Config {
            url: url.into(),
            token: "test-token".into(),
            recall_timeout,
            logger: Arc::new(Logger::discard()),
        })
        .unwrap()
    }

    fn user() -> Scope {
        Scope {
            namespace: "user".into(),
            id: "local".into(),
        }
    }

    fn workspace() -> Scope {
        Scope {
            namespace: "workspace".into(),
            id: "sha256:0123456789abcdef".into(),
        }
    }

    fn context(origin: Origin) -> MemoryContext {
        MemoryContext {
            principal: "principal".into(),
            scopes: vec![user(), workspace()],
            write_scope: Some(workspace()),
            workspace_id: None,
            session_id: Some("s-1".into()),
            task_id: None,
            origin,
        }
    }

    fn client(service: RemoteService) -> Client {
        Client::with_learning(
            Arc::new(service),
            LearningGrant {
                scopes: vec![user(), workspace()],
                input_classes: vec!["explicit_memory".into()],
                background: false,
            },
        )
        .expect("client admits the remote description")
    }

    fn recall_request(limit: usize, budget: usize) -> RecallRequest {
        RecallRequest {
            context: context(Origin::Human),
            query: "editor".into(),
            limit,
            token_budget: budget,
        }
    }

    fn item(id: &str, scope: &Scope, text: &str) -> Value {
        json!({"id": id, "scope": scope, "text": text})
    }

    fn complete(items: Vec<Value>) -> Value {
        json!({"coverage": "complete", "items": items})
    }

    async fn recall_kind(answer: Reply, limit: usize, budget: usize) -> ErrorKind {
        let answer = Mutex::new(Some(answer));
        let (url, _) = serve(move |_| answer.lock().unwrap().take().unwrap()).await;
        client(service(&url, Duration::from_secs(5)))
            .recall(&recall_request(limit, budget), &CancellationToken::new())
            .await
            .expect_err("result must be discarded")
            .kind
    }

    #[tokio::test]
    async fn complete_recall_returns_items_with_references() {
        let (url, _) = serve(|_| {
            reply(
                200,
                complete(vec![
                    item("m-1", &user(), "likes vim"),
                    item("m-2", &workspace(), "uses tabs"),
                ]),
            )
        })
        .await;
        let result = client(service(&url, Duration::from_secs(5)))
            .recall(&recall_request(12, 8192), &CancellationToken::new())
            .await
            .expect("recall");
        assert_eq!(result.coverage, Coverage::Complete);
        assert_eq!(result.items.len(), 2);
        let reference = result.items[0].reference.as_ref().unwrap();
        assert_eq!(
            (reference.service.as_str(), reference.id.as_str()),
            ("remote", "m-1")
        );
        assert_eq!(result.items[0].content, Content::Text("likes vim".into()));
    }

    #[tokio::test]
    async fn recall_request_carries_headers_and_wire_body() {
        let (url, seen) = serve(|_| reply(200, complete(vec![]))).await;
        client(service(&url, Duration::from_secs(5)))
            .recall(&recall_request(12, 4000), &CancellationToken::new())
            .await
            .expect("recall");
        let seen = seen.lock().unwrap();
        let (path, headers, body) = &seen[0];
        assert_eq!(path, "/v1/recall");
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert_eq!(headers["otto-memory-protocol"], "1.0");
        assert_eq!(
            headers["user-agent"],
            concat!("otto/", env!("CARGO_PKG_VERSION"))
        );
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["max_bytes"], 4000);
        assert_eq!(body["limit"], 12);
        assert_eq!(body["query"], "editor");
        assert_eq!(body["context"]["session_id"], "s-1");
        assert_eq!(body["context"]["scopes"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn recall_discards_results_that_break_a_rule() {
        let ok = || item("m-1", &user(), "likes vim");
        // Coverage other than complete.
        for coverage in ["partial", "unknown"] {
            let body = json!({"coverage": coverage, "items": [ok()]});
            assert_eq!(
                recall_kind(reply(200, body), 12, 8192).await,
                ErrorKind::Unavailable
            );
        }
        // Unknown coverage value.
        let body = json!({"coverage": "most", "items": []});
        assert_eq!(
            recall_kind(reply(200, body), 12, 8192).await,
            ErrorKind::Unavailable
        );
        // Missing and foreign protocol headers.
        for protocol in [None, Some("2.0"), Some("1."), Some("x1.0")] {
            let mut answer = reply(200, complete(vec![ok()]));
            answer.protocol = protocol;
            assert_eq!(recall_kind(answer, 12, 8192).await, ErrorKind::Unavailable);
        }
        // Scope outside the context.
        let foreign = Scope {
            namespace: "user".into(),
            id: "other".into(),
        };
        let body = complete(vec![ok(), item("m-2", &foreign, "x")]);
        assert_eq!(
            recall_kind(reply(200, body), 12, 8192).await,
            ErrorKind::Unavailable
        );
        // More items than the limit.
        let body = complete(vec![ok(), item("m-2", &user(), "b")]);
        assert_eq!(
            recall_kind(reply(200, body), 1, 8192).await,
            ErrorKind::Unavailable
        );
        // Empty text, oversize text, non-printable id.
        for bad in [
            item("m-1", &user(), ""),
            item("m-1", &user(), &"a".repeat(8193)),
            item("m id", &user(), "x"),
        ] {
            let body = complete(vec![bad]);
            assert_eq!(
                recall_kind(reply(200, body), 12, 8192).await,
                ErrorKind::Unavailable
            );
        }
        // Rendering over max_bytes: the preamble alone is 131 bytes.
        let body = complete(vec![item("m-1", &user(), &"a".repeat(500))]);
        assert_eq!(
            recall_kind(reply(200, body), 12, 400).await,
            ErrorKind::Unavailable
        );
    }

    #[tokio::test]
    async fn recall_accepts_rendering_exactly_at_max_bytes() {
        let records = [Record {
            id: "m-1".into(),
            scope: agent::Scope {
                namespace: "user".into(),
                id: "local".into(),
            },
            kind: "reference".into(),
            key: String::new(),
            text: "likes vim".into(),
            standing: false,
        }];
        let exact = render_memory_context(&records).len();
        let (url, _) =
            serve(|_| reply(200, complete(vec![item("m-1", &user(), "likes vim")]))).await;
        let client = client(service(&url, Duration::from_secs(5)));
        let control = CancellationToken::new();
        assert!(
            client
                .recall(&recall_request(12, exact), &control)
                .await
                .is_ok()
        );
        assert!(
            client
                .recall(&recall_request(12, exact - 1), &control)
                .await
                .is_err()
        );
    }

    /// `recall_rendered.txt` is the protocol 1 rendering of
    /// `recall_response.json`; its byte length is what `max_bytes` bounds.
    #[tokio::test]
    async fn recall_response_fixture_renders_to_the_golden_text() {
        let fixture = include_str!("fixtures/v1/recall_response.json");
        let rendered = include_str!("fixtures/v1/recall_rendered.txt");
        let response: wire::RecallResponse = serde_json::from_str(fixture).unwrap();
        let records: Vec<_> = response.items.iter().map(record).collect();
        assert_eq!(render_memory_context(&records), rendered);
        let (url, _) = serve(move |_| Reply {
            status: 200,
            protocol: Some("1.0"),
            body: fixture.as_bytes().to_vec(),
            delay: Duration::ZERO,
        })
        .await;
        let result = client(service(&url, Duration::from_secs(5)))
            .recall(
                &recall_request(12, rendered.len()),
                &CancellationToken::new(),
            )
            .await
            .expect("recall at exactly the rendered size");
        assert_eq!(result.used_tokens, Some(rendered.len()));
    }

    #[tokio::test]
    async fn recall_rejects_body_over_one_mebibyte() {
        let mut answer = reply(200, complete(vec![]));
        answer.body.resize(MAX_RESPONSE_BYTES + 1, b' ');
        assert_eq!(recall_kind(answer, 12, 8192).await, ErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn recall_times_out_without_items() {
        let mut answer = reply(200, complete(vec![item("m-1", &user(), "x")]));
        answer.delay = Duration::from_millis(500);
        let (url, _) = serve(move |_| Reply {
            body: answer.body.clone(),
            ..answer
        })
        .await;
        let error = client(service(&url, Duration::from_millis(100)))
            .recall(&recall_request(12, 8192), &CancellationToken::new())
            .await
            .expect_err("timeout");
        assert_eq!(error.kind, ErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn cancellation_stops_the_request() {
        let mut answer = reply(200, complete(vec![]));
        answer.delay = Duration::from_secs(5);
        let (url, _) = serve(move |_| Reply {
            body: answer.body.clone(),
            ..answer
        })
        .await;
        let service = service(&url, Duration::from_secs(10));
        let control = CancellationToken::new();
        let cancel = control.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let error = service
            .recall(&recall_request(12, 8192), &control)
            .await
            .expect_err("canceled");
        assert_eq!(error.kind, ErrorKind::Canceled);
    }

    #[tokio::test]
    async fn connect_failure_is_unavailable_for_reads_and_writes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = client(service(&url, Duration::from_secs(1)));
        let control = CancellationToken::new();
        assert_eq!(
            client
                .recall(&recall_request(12, 8192), &control)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
        assert_eq!(
            client
                .remember(&remember_request(None), &control)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unavailable
        );
    }

    fn remember_request(target: Option<MemoryRef>) -> RememberRequest {
        RememberRequest {
            context: context(Origin::Model),
            request_id: "0123456789abcdef0123456789abcdef".into(),
            content: Content::Text("likes vim".into()),
            target,
            metadata: [("kind".to_string(), "preference".to_string())].into(),
        }
    }

    fn forget_request() -> ForgetRequest {
        ForgetRequest {
            context: context(Origin::Human),
            request_id: "fedcba9876543210fedcba9876543210".into(),
            target: ForgetTarget::Item(MemoryRef {
                service: "remote".into(),
                scope: user(),
                id: "m-17".into(),
                revision: Some("4".into()),
            }),
            coverage: ForgetLevel::Suppress,
        }
    }

    #[tokio::test]
    async fn remember_sends_the_wire_body_and_maps_applied() {
        let (url, seen) = serve(|_| {
            reply(
                200,
                json!({"state": "applied", "id": "m-9", "revision": "1"}),
            )
        })
        .await;
        let receipt = client(service(&url, Duration::from_secs(5)))
            .remember(&remember_request(None), &CancellationToken::new())
            .await
            .expect("remember");
        let reference = receipt.items[0].reference.as_ref().unwrap();
        assert_eq!(reference.scope, workspace());
        assert_eq!(reference.revision.as_deref(), Some("1"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/v1/remember");
        let body: Value = serde_json::from_slice(&seen[0].2).unwrap();
        assert_eq!(body["origin"], "model");
        assert_eq!(body["scope"], json!(workspace()));
        assert_eq!(body["text"], "likes vim");
        assert_eq!(body["target"], Value::Null);
        assert_eq!(body["metadata"], json!({"kind": "preference"}));
        assert_eq!(body["request_id"], "0123456789abcdef0123456789abcdef");
    }

    #[tokio::test]
    async fn remember_with_target_uses_the_target_scope() {
        let (url, seen) = serve(|_| reply(200, json!({"state": "applied", "id": "m-17"}))).await;
        let target = MemoryRef {
            service: "remote".into(),
            scope: user(),
            id: "m-17".into(),
            revision: Some("4".into()),
        };
        client(service(&url, Duration::from_secs(5)))
            .remember(&remember_request(Some(target)), &CancellationToken::new())
            .await
            .expect("remember");
        let body: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
        assert_eq!(body["scope"], json!(user()));
        assert_eq!(body["target"], json!({"id": "m-17", "revision": "4"}));
    }

    #[tokio::test]
    async fn remember_receipts_map_each_wire_state() {
        let cases = [
            (
                "applied",
                Some("m-1"),
                (
                    Admission::Acknowledged,
                    Completion::Complete,
                    Visibility::Ready,
                    ItemState::Applied,
                ),
            ),
            (
                "no_change",
                Some("m-1"),
                (
                    Admission::Acknowledged,
                    Completion::Complete,
                    Visibility::Ready,
                    ItemState::NoChange,
                ),
            ),
            (
                "skipped",
                None,
                (
                    Admission::Acknowledged,
                    Completion::Complete,
                    Visibility::NotApplicable,
                    ItemState::Skipped,
                ),
            ),
            (
                "pending",
                None,
                (
                    Admission::Acknowledged,
                    Completion::Pending,
                    Visibility::Pending,
                    ItemState::Unknown,
                ),
            ),
            (
                "failed",
                None,
                (
                    Admission::Rejected,
                    Completion::Failed,
                    Visibility::NotApplicable,
                    ItemState::Failed,
                ),
            ),
        ];
        for (state, id, expected) in cases {
            let body = json!({"state": state, "id": id});
            let (url, _) = serve(move |_| reply(200, body.clone())).await;
            let receipt = client(service(&url, Duration::from_secs(5)))
                .remember(&remember_request(None), &CancellationToken::new())
                .await
                .unwrap_or_else(|error| panic!("{state}: {error}"));
            let got = (
                receipt.admission,
                receipt.completion,
                receipt.visibility,
                receipt.items[0].state,
            );
            assert_eq!(got, expected, "{state}");
            assert_eq!(
                receipt.items[0].reference.is_some(),
                id.is_some(),
                "{state}"
            );
        }
    }

    #[tokio::test]
    async fn forget_receipts_map_each_wire_state() {
        let cases = [
            (
                "applied",
                Some("m-17"),
                Completion::Complete,
                ItemState::Applied,
            ),
            (
                "no_change",
                Some("m-17"),
                Completion::Complete,
                ItemState::NoChange,
            ),
            ("skipped", None, Completion::Partial, ItemState::Skipped),
            ("pending", None, Completion::Pending, ItemState::Unknown),
            ("failed", None, Completion::Failed, ItemState::Failed),
        ];
        for (state, id, completion, item) in cases {
            let body = json!({"state": state, "id": id});
            let (url, seen) = serve(move |_| reply(200, body.clone())).await;
            let receipt = client(service(&url, Duration::from_secs(5)))
                .forget(&forget_request(), &CancellationToken::new())
                .await
                .unwrap_or_else(|error| panic!("{state}: {error}"));
            assert_eq!(
                (receipt.completion, receipt.items[0].state),
                (completion, item),
                "{state}"
            );
            let seen = seen.lock().unwrap();
            assert_eq!(seen[0].0, "/v1/forget");
            let sent: Value = serde_json::from_slice(&seen[0].2).unwrap();
            assert_eq!(sent["origin"], "human");
            assert_eq!(sent["id"], "m-17");
            assert_eq!(sent["revision"], "4");
            assert_eq!(sent["scope"], json!(user()));
        }
    }

    #[tokio::test]
    async fn applied_receipt_without_id_is_an_unknown_outcome() {
        let (url, _) = serve(|_| reply(200, json!({"state": "applied"}))).await;
        let error = client(service(&url, Duration::from_secs(5)))
            .remember(&remember_request(None), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
    }

    #[tokio::test]
    async fn write_timeout_after_send_is_an_unknown_outcome() {
        let mut answer = reply(200, json!({"state": "applied", "id": "m-1"}));
        answer.delay = Duration::from_millis(500);
        let (url, _) = serve(move |_| Reply {
            body: answer.body.clone(),
            ..answer
        })
        .await;
        let mut service = service(&url, Duration::from_secs(5));
        service.write_timeout = Duration::from_millis(100);
        let error = client(service)
            .remember(&remember_request(None), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
    }

    #[tokio::test]
    async fn error_table_maps_status_and_code() {
        let table = [
            (400, "invalid_input", ErrorKind::InvalidInput),
            (401, "unauthorized", ErrorKind::Unauthorized),
            (403, "forbidden", ErrorKind::Unauthorized),
            (409, "conflict", ErrorKind::Conflict),
            (413, "too_large", ErrorKind::InvalidInput),
            (429, "rate_limited", ErrorKind::RateLimited),
            (503, "unavailable", ErrorKind::Unavailable),
            (400, "unsupported_version", ErrorKind::Unsupported),
            (500, "invalid_input", ErrorKind::Unavailable),
            (401, "made_up", ErrorKind::Unavailable),
        ];
        for (status, code, kind) in table {
            let body = json!({"error": {"code": code, "message": "secret detail xyz"}});
            let (url, _) = serve(move |_| reply(status, body.clone())).await;
            let service = service(&url, Duration::from_secs(5));
            let error = service
                .recall(&recall_request(12, 8192), &CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "{status} {code}");
            assert!(!error.to_string().contains("xyz"), "{status} {code}");
            assert!(!format!("{error:?}").contains("xyz"), "{status} {code}");
        }
    }

    #[tokio::test]
    async fn unparseable_error_body_is_unavailable() {
        let (url, _) = serve(|_| Reply {
            status: 502,
            protocol: Some("1.0"),
            body: b"<html>bad gateway</html>".to_vec(),
            delay: Duration::ZERO,
        })
        .await;
        let error = service(&url, Duration::from_secs(5))
            .recall(&recall_request(12, 8192), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unavailable);
    }

    fn get_request(revision: Option<&str>) -> InspectRequest {
        InspectRequest {
            context: context(Origin::Human),
            query: InspectQuery::Get(MemoryRef {
                service: "remote".into(),
                scope: user(),
                id: "m-17".into(),
                revision: revision.map(Into::into),
            }),
        }
    }

    #[tokio::test]
    async fn get_returns_one_item_and_404_returns_none() {
        let (url, seen) = serve(|_| {
            reply(
                200,
                json!({"item": {"id": "m-17", "scope": user(), "revision": "4", "text": "likes vim"}}),
            )
        })
        .await;
        let client = client(service(&url, Duration::from_secs(5)));
        let control = CancellationToken::new();
        let result = client
            .inspect(&get_request(Some("4")), &control)
            .await
            .expect("get");
        assert_eq!(result.items.len(), 1);
        assert_eq!(seen.lock().unwrap()[0].0, "/v1/get");
        let sent: Value = serde_json::from_slice(&seen.lock().unwrap()[0].2).unwrap();
        assert_eq!(sent["id"], "m-17");
        assert_eq!(sent["scope"], json!(user()));

        let (url, _) = serve(|_| Reply {
            status: 404,
            protocol: Some("1.0"),
            body: Vec::new(),
            delay: Duration::ZERO,
        })
        .await;
        let result = self::client(service(&url, Duration::from_secs(5)))
            .inspect(&get_request(None), &control)
            .await
            .expect("404");
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn get_discards_an_item_for_another_id_or_scope() {
        for item in [
            json!({"id": "m-99", "scope": user(), "text": "x"}),
            json!({"id": "m-17", "scope": workspace(), "text": "x"}),
        ] {
            let (url, _) = serve(move |_| reply(200, json!({"item": item.clone()}))).await;
            let error = service(&url, Duration::from_secs(5))
                .inspect(&get_request(None), &CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Unavailable);
        }
    }

    #[tokio::test]
    async fn unsupported_operations_are_refused_without_a_request() {
        let (url, seen) = serve(|_| reply(200, json!({}))).await;
        let service = service(&url, Duration::from_secs(5));
        let control = CancellationToken::new();
        let mut request = get_request(None);
        request.query = InspectQuery::Status;
        assert_eq!(
            service.inspect(&request, &control).await.unwrap_err().kind,
            ErrorKind::Unsupported
        );
        let mut forget = forget_request();
        forget.coverage = ForgetLevel::DeleteActive;
        assert_eq!(
            service.forget(&forget, &control).await.unwrap_err().kind,
            ErrorKind::Unsupported
        );
        forget.coverage = ForgetLevel::Suppress;
        forget.target = ForgetTarget::Scope(user());
        assert_eq!(
            service.forget(&forget, &control).await.unwrap_err().kind,
            ErrorKind::Unsupported
        );
        let mut remember = remember_request(None);
        remember.content = Content::Resource {
            uri: "file".into(),
            media_type: "text".into(),
        };
        assert_eq!(
            service
                .remember(&remember, &control)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
        remember.content = Content::Text("x".into());
        remember.context.origin = Origin::Extractor;
        assert_eq!(
            service
                .remember(&remember, &control)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn constructor_and_describe_make_no_request() {
        let (url, seen) = serve(|_| reply(200, json!({}))).await;
        let service = service(&url, Duration::from_secs(5));
        let description = service.describe();
        assert_eq!(description.identity, "remote");
        assert_eq!(description.encryption_at_rest, None);
        assert!(seen.lock().unwrap().is_empty());
        assert!(
            RemoteService::new(Config {
                url: "ftp://x".into(),
                token: "test-token".into(),
                recall_timeout: Duration::from_secs(1),
                logger: Arc::new(Logger::discard()),
            })
            .is_err()
        );
    }

    fn turn(text: String) -> wire::TurnRequest {
        wire::TurnRequest {
            context: wire::Context {
                scopes: vec![user()],
                session_id: None,
            },
            turn_id: "00112233445566778899aabbccddeeff".into(),
            status: wire::TurnStatus::Success,
            messages: vec![wire::TurnMessage {
                id: "msg-1".into(),
                role: wire::TurnRole::User,
                created_at: "2026-10-17T08:00:00Z".parse().unwrap(),
                blocks: vec![wire::TurnBlock::Text { text }],
            }],
        }
    }

    #[tokio::test]
    async fn upload_turn_sends_the_turn_and_accepts_202() {
        let (url, seen) = serve(|_| reply(202, json!({}))).await;
        let service = service(&url, Duration::from_secs(5));
        service
            .upload_turn(&turn("hello".into()))
            .await
            .expect("202");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/v1/turns");
        assert_eq!(seen[0].1["authorization"], "Bearer test-token");
        let body: Value = serde_json::from_slice(&seen[0].2).unwrap();
        assert_eq!(body["status"], "success");
        assert_eq!(
            body["messages"][0]["blocks"][0],
            json!({"type": "text", "text": "hello"})
        );
        assert_eq!(body["messages"][0]["created_at"], "2026-10-17T08:00:00Z");
    }

    #[tokio::test]
    async fn upload_turn_maps_errors_and_refuses_over_four_mebibytes() {
        let (url, seen) = serve(|_| {
            reply(
                503,
                json!({"error": {"code": "unavailable", "message": "later"}}),
            )
        })
        .await;
        let service = service(&url, Duration::from_secs(5));
        let error = service
            .upload_turn(&turn("hello".into()))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(seen.lock().unwrap().len(), 1);

        let error = service
            .upload_turn(&turn("a".repeat(MAX_TURN_BYTES)))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidInput);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
