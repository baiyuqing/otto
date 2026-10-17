//! HTTP client for a remote memory service speaking protocol 1.x
//! (`docs/specs/2026-10-17-remote-memory-scope-redesign.md`). The service is
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
        if config.recall_timeout.is_zero() || config.recall_timeout > Duration::from_secs(10) {
            return Err(
                "invalid memory recall timeout: must be greater than zero and at most 10s".into(),
            );
        }
        let url = reqwest::Url::parse(&config.url).map_err(|_| "invalid memory service url")?;
        let loopback = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
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

    /// POST /v1/turns, 30 s timeout. Only 202 with an empty object succeeds.
    pub async fn upload_turn(&self, turn: &wire::TurnRequest) -> Result<()> {
        let body = serde_json::to_vec(turn).map_err(|_| err(ErrorKind::InvalidInput))?;
        if body.len() > MAX_TURN_BYTES {
            return Err(err(ErrorKind::InvalidInput));
        }
        validate_wire_context(&turn.context)?;
        if !valid_request_id(&turn.turn_id) {
            return Err(err(ErrorKind::InvalidInput));
        }
        let raw = self
            .exchange(
                "turns",
                body,
                TURN_TIMEOUT,
                false,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await?
            .into_success(self, "turns")?;
        if raw.status != 202 || !empty_object(&raw.body) {
            return Err(err(ErrorKind::Unavailable));
        }
        Ok(())
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
        let expected = if write {
            raw.status == 200 || raw.status == 202
        } else {
            raw.status == 200
        };
        if !expected {
            return Err(err(if write {
                ErrorKind::OutcomeUnknown
            } else {
                ErrorKind::Unavailable
            }));
        }
        let decoded: R = wire::decode(&raw.body).map_err(|_| {
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
        })?;
        if write {
            let receipt: wire::Receipt =
                wire::decode(&raw.body).map_err(|_| err(ErrorKind::OutcomeUnknown))?;
            if (raw.status == 202) != (receipt.state == wire::ReceiptState::Pending) {
                return Err(err(ErrorKind::OutcomeUnknown));
            }
        }
        Ok(decoded)
    }

    /// Converts validated service data into an explicit, non-metadata projection.
    fn item(
        item: wire::Item,
        context: &MemoryContext,
        mandatory: bool,
        view: MemoryView,
    ) -> Option<RecallItem> {
        let scope: Scope = item.scope.into();
        if context.validate_scope(&scope).is_err()
            || !valid_opaque_id(&item.id, MAX_ID_BYTES)
            || !valid_opaque_id(&item.revision, MAX_ID_BYTES)
            || !item.key.as_deref().is_none_or(valid_memory_key)
            || Content::Text(item.text.clone()).validate().is_err()
        {
            return None;
        }
        let organization_id = context.organization_id.clone()?;
        Some(RecallItem {
            remote: Some(RemoteProjection {
                organization_id: organization_id.clone(),
                revision: item.revision.clone(),
                key: item.key,
                pinned: item.pinned,
                mandatory,
                view,
            }),
            reference: Some(MemoryRef {
                organization_id: Some(organization_id),
                service: IDENTITY.into(),
                scope,
                id: item.id,
                revision: Some(item.revision),
            }),
            content: Content::Text(item.text),
            sources: Vec::new(),
            effective_at: None,
            metadata: Default::default(),
        })
    }

    pub(crate) fn context(context: &MemoryContext) -> Result<wire::Context> {
        context.validate()?;
        let result = wire::Context {
            organization_id: context
                .organization_id
                .clone()
                .ok_or_else(|| err(ErrorKind::InvalidInput))?,
            user_id: context.principal.clone(),
            workspace_id: context.workspace_id.clone(),
            session_id: context.session_id.clone(),
        };
        validate_wire_context(&result)?;
        let expected = derived_scopes(&result);
        if context.scopes.len() != expected.len()
            || context.scopes.iter().any(|scope| !expected.contains(scope))
        {
            return Err(err(ErrorKind::Unauthorized));
        }
        Ok(result)
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
        context: &MemoryContext,
        target: Option<&MemoryRef>,
    ) -> Result<Receipt> {
        use wire::ReceiptState::*;
        let invalid = || {
            self.warn(endpoint, 200, "invalid_response", "invalid receipt");
            err(ErrorKind::OutcomeUnknown)
        };
        if wire.request_id != request_id {
            return Err(invalid());
        }
        let terminal_item = matches!(wire.state, Applied | NoChange);
        if terminal_item != wire.item.is_some() {
            return Err(invalid());
        }
        let reference = match wire.item {
            Some(item) => {
                let item_scope: Scope = item.scope.into();
                if item.organization_id != context.organization_id.as_deref().unwrap_or_default()
                    || item_scope != *scope
                    || !valid_opaque_id(&item.id, MAX_ID_BYTES)
                    || !valid_opaque_id(&item.revision, MAX_ID_BYTES)
                    || target.is_some_and(|target| target.id != item.id)
                {
                    return Err(invalid());
                }
                Some(MemoryRef {
                    organization_id: Some(item.organization_id),
                    service: IDENTITY.into(),
                    scope: item_scope,
                    id: item.id,
                    revision: Some(item.revision),
                })
            }
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
            Pending | Unknown => (
                Admission::Acknowledged,
                if wire.state == Pending {
                    Completion::Pending
                } else {
                    Completion::Unknown
                },
                if wire.state == Pending {
                    Visibility::Pending
                } else {
                    Visibility::Unknown
                },
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
        let parsed = wire::decode::<wire::ErrorBody>(&self.body).ok();
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
        (409, "conflict" | "idempotency_conflict") => ErrorKind::Conflict,
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

fn record(item: &RecallItem) -> Record {
    let reference = item.reference.as_ref().expect("remote item reference");
    Record {
        remote: item.remote.clone(),
        id: reference.id.clone(),
        scope: agent::Scope {
            namespace: reference.scope.namespace.clone(),
            id: reference.scope.id.clone(),
        },
        kind: "reference".into(),
        key: String::new(),
        text: match &item.content {
            Content::Text(text) => text.clone(),
            _ => unreachable!(),
        },
        standing: false,
    }
}

fn valid_request_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn empty_object(bytes: &[u8]) -> bool {
    wire::decode::<serde_json::Value>(bytes)
        .is_ok_and(|value| value.as_object().is_some_and(|object| object.is_empty()))
}
fn validate_wire_context(context: &wire::Context) -> Result<()> {
    if !valid_organization_id(&context.organization_id)
        || !valid_opaque_id(&context.user_id, 128)
        || context
            .workspace_id
            .as_deref()
            .is_some_and(|id| !valid_opaque_id(id, 128))
        || context
            .session_id
            .as_deref()
            .is_some_and(|id| !valid_opaque_id(id, MAX_ID_BYTES))
    {
        return Err(err(ErrorKind::InvalidInput));
    }
    Ok(())
}
fn derived_scopes(context: &wire::Context) -> Vec<Scope> {
    let mut scopes = vec![
        Scope {
            namespace: "organization".into(),
            id: context.organization_id.clone(),
        },
        Scope {
            namespace: "user".into(),
            id: context.user_id.clone(),
        },
    ];
    if let Some(id) = &context.workspace_id {
        scopes.push(Scope {
            namespace: "workspace".into(),
            id: id.clone(),
        });
    }
    scopes
}
fn check_target(context: &MemoryContext, reference: &MemoryRef) -> Result<()> {
    reference.validate(context, IDENTITY)
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
        request.validate()?;
        if request.query.trim().is_empty() {
            return Err(err(ErrorKind::InvalidInput));
        }
        let context = Self::context(&request.context)?;
        let body = wire::RecallBody {
            context: context.clone(),
            query: request.query.clone(),
            scope_filter: request.scope_filter.clone(),
            limit: request.limit,
            max_bytes: request.token_budget,
        };
        let response: wire::RecallResponse = self
            .call("recall", &body, self.recall_timeout, false, control)
            .await?;
        let rejected = || err(ErrorKind::Unavailable);
        if response.context != context
            || response.coverage != wire::Coverage::Complete
            || response.items.len() > request.limit
        {
            return Err(rejected());
        }
        let mut items = Vec::with_capacity(response.items.len());
        let mut refs = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        let mut last_group = true;
        let mut last_priority = u8::MAX;
        for recalled in response.items {
            let item = recalled.item;
            let priority = item.scope.namespace.priority();
            if request
                .scope_filter
                .as_ref()
                .is_some_and(|filter| !filter.contains(&item.scope.namespace))
                || !refs.insert((item.scope.clone(), item.id.clone()))
                || item
                    .key
                    .as_ref()
                    .is_some_and(|key| !keys.insert(key.clone()))
                || (!last_group && recalled.mandatory)
                || (last_group == recalled.mandatory && priority > last_priority)
                || (item.pinned && !recalled.mandatory)
            {
                return Err(rejected());
            }
            last_group = recalled.mandatory;
            last_priority = priority;
            items.push(
                Self::item(
                    item,
                    &request.context,
                    recalled.mandatory,
                    MemoryView::Effective,
                )
                .ok_or_else(rejected)?,
            );
        }
        let records: Vec<_> = items.iter().map(record).collect();
        let used = render_memory_context(&records).len();
        if used > request.token_budget {
            return Err(rejected());
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
        let context = Self::context(&request.context)?;
        check_target(&request.context, reference)?;
        let body = serde_json::to_vec(&wire::GetBody {
            context,
            scope: (&reference.scope).into(),
            id: reference.id.clone(),
        })
        .map_err(|_| err(ErrorKind::InvalidInput))?;
        let raw = self
            .exchange("get", body, self.recall_timeout, false, control)
            .await?;
        let mut result = InspectResult {
            items: Vec::new(),
            next_cursor: String::new(),
            state: "raw".into(),
        };
        if raw.status == 404 {
            if !empty_object(&raw.body) {
                return Err(err(ErrorKind::Unavailable));
            }
            return Ok(result);
        }
        let raw = raw.into_success(self, "get")?;
        let invalid = || err(ErrorKind::Unavailable);
        if raw.status != 200 {
            return Err(invalid());
        }
        let response: wire::GetResponse = wire::decode(&raw.body).map_err(|_| invalid())?;
        if response.organization_id
            != request
                .context
                .organization_id
                .as_deref()
                .unwrap_or_default()
            || response.item.id != reference.id
            || Scope::from(response.item.scope.clone()) != reference.scope
        {
            return Err(invalid());
        }
        result.items.push(
            Self::item(response.item, &request.context, false, MemoryView::Raw)
                .ok_or_else(invalid)?,
        );
        Ok(result)
    }

    async fn remember(
        &self,
        request: &RememberRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate(IDENTITY)?;
        let context = Self::context(&request.context)?;
        if !valid_request_id(&request.request_id) {
            return Err(err(ErrorKind::InvalidInput));
        }
        if let Some(target) = &request.target {
            check_target(&request.context, target)?;
        }
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
        request.context.validate_scope(&scope)?;
        if scope.namespace == "organization" && request.context.origin != Origin::Human {
            return Err(err(ErrorKind::Unauthorized));
        }
        let body = wire::RememberBody {
            context,
            key: request.key.clone(),
            request_id: request.request_id.clone(),
            origin: Self::origin(&request.context)?,
            scope: (&scope).into(),
            text: text.clone(),
            target: request.target.as_ref().map(|target| wire::Target {
                id: target.id.clone(),
                revision: target.revision.clone().expect("validated revision"),
            }),
            metadata: request.metadata.clone(),
        };
        let receipt: wire::Receipt = self
            .call("remember", &body, self.write_timeout, true, control)
            .await?;
        self.receipt(
            "remember",
            &request.request_id,
            &scope,
            receipt,
            false,
            &request.context,
            request.target.as_ref(),
        )
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
        request.validate(IDENTITY)?;
        let context = Self::context(&request.context)?;
        check_target(&request.context, target)?;
        if !valid_request_id(&request.request_id) {
            return Err(err(ErrorKind::InvalidInput));
        }
        if target.scope.namespace == "organization" {
            return Err(err(ErrorKind::Unauthorized));
        }
        let body = wire::ForgetBody {
            context,
            request_id: request.request_id.clone(),
            origin: Self::origin(&request.context)?,
            scope: (&target.scope).into(),
            id: target.id.clone(),
            revision: target.revision.clone().expect("validated revision"),
        };
        let receipt: wire::Receipt = self
            .call("forget", &body, self.write_timeout, true, control)
            .await?;
        self.receipt(
            "forget",
            &request.request_id,
            &target.scope,
            receipt,
            true,
            &request.context,
            Some(target),
        )
    }
}

#[cfg(test)]
mod tests;
