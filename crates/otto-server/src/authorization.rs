//! Authorization of API operations.
//!
//! Every request that reaches a `/v1/` handler carries a [`Principal`],
//! attached by the token gate in `middleware::instrument` or by the router a
//! transport was given ([`crate::Server::router_for`]). [`authorize_operation`]
//! runs after the token gate and before the handler's extractors, looks the
//! request up in the [`OperationId`] registry and calls [`authorize`].
//!
//! Policy: the owner may perform every operation; any other principal is
//! denied. Production authentication produces only [`Principal::Owner`], so
//! this module changes no behavior for existing clients. The design record is
//! `docs/specs/2026-11-14-api-authorization-scaffold.md`.

mod operations;

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, MatchedPath, RawPathParams, State};
use axum::handler::Handler;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{MethodFilter, MethodRouter, on};

pub use operations::OperationId;

use crate::{Server, error_response};

/// The authenticated caller of an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// The account Otto runs as. Every caller authenticated by
    /// `OTTO_SERVER_TOKEN` or the Unix socket.
    Owner,
    /// A named client with fewer permissions. No transport produces one yet;
    /// tests construct it to exercise the deny path.
    Client { name: String },
}

impl Principal {
    /// The name logged for this principal.
    pub fn name(&self) -> &str {
        match self {
            Principal::Owner => "owner",
            Principal::Client { name } => name,
        }
    }
}

/// Why [`authorize`] refused an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// The request carries no principal.
    NoPrincipal,
    /// The principal may not perform this operation.
    NotPermitted,
}

/// Decides whether `principal` may perform `operation`. `path_params` are the
/// route's path parameters, the operation's targets named in the path.
pub fn authorize(
    principal: Option<&Principal>,
    operation: OperationId,
    path_params: &[(String, String)],
) -> Result<(), Denied> {
    // The targets are part of the decision once per-client permissions exist;
    // the owner/other policy does not need them.
    let _ = (operation, path_params);
    match principal {
        Some(Principal::Owner) => Ok(()),
        Some(Principal::Client { .. }) => Err(Denied::NotPermitted),
        None => Err(Denied::NoPrincipal),
    }
}

/// Builds the API router from [`OperationId`]s: each handler is registered
/// under its operation's method and path, so a route added through this type
/// always has an id.
pub(crate) struct OperationRoutes {
    router: Router<Arc<Server>>,
    registered: Vec<OperationId>,
}

impl OperationRoutes {
    pub(crate) fn new() -> Self {
        Self {
            router: Router::new(),
            registered: Vec::new(),
        }
    }

    /// Registers `handler` for `operation`'s method and path.
    pub(crate) fn add<H, T>(self, operation: OperationId, handler: H) -> Self
    where
        H: Handler<T, Arc<Server>>,
        T: 'static,
    {
        let filter =
            MethodFilter::try_from(operation.method()).expect("every operation method is routable");
        self.add_method_router(operation, on(filter, handler))
    }

    /// Registers a prepared method router, for a handler that needs its own
    /// layer. `method_router` must serve exactly `operation`'s method.
    pub(crate) fn add_method_router(
        mut self,
        operation: OperationId,
        method_router: MethodRouter<Arc<Server>>,
    ) -> Self {
        self.router = self.router.route(operation.path(), method_router);
        self.registered.push(operation);
        self
    }

    /// The operations registered so far, in registration order.
    #[cfg(test)]
    pub(crate) fn registered(&self) -> &[OperationId] {
        &self.registered
    }

    pub(crate) fn into_router(self) -> Router<Arc<Server>> {
        self.router
    }
}

/// The request id `instrument` assigned, for log lines written further in.
#[derive(Clone)]
pub(crate) struct RequestId(pub(crate) String);

/// Authorizes the matched operation before the handler runs.
///
/// - An unmatched request passes; the router answers it with its fallback.
/// - A matched route pattern that no operation uses was added without the
///   registry and is refused with `500 unregistered_operation`.
/// - A method the pattern does not serve passes, so the router answers `405`.
///   `HEAD` is authorized as `GET`, because axum serves it with the `GET`
///   handler.
/// - Otherwise [`authorize`] decides; a refusal answers `403 forbidden` and the
///   handler does not run.
pub(crate) async fn authorize_operation(
    State(server): State<Arc<Server>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(matched) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string())
    else {
        return next.run(request).await;
    };
    let candidates = OperationId::for_path(&matched);
    if candidates.is_empty() {
        server.log.warn(
            "authorization_unregistered_operation",
            &[("route", matched.clone())],
        );
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unregistered_operation",
            "route has no registered operation",
        );
    }
    let method = match request.method() {
        &Method::HEAD => Method::GET,
        method => method.clone(),
    };
    let Some(operation) = candidates
        .iter()
        .copied()
        .find(|operation| operation.method() == method)
    else {
        return next.run(request).await;
    };
    if !operation.requires_principal() {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let path_params: Vec<(String, String)> = RawPathParams::from_request_parts(&mut parts, &())
        .await
        .map(|params| {
            params
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let principal = parts.extensions.get::<Principal>().cloned();
    match authorize(principal.as_ref(), operation, &path_params) {
        Ok(()) => {
            next.run(axum::extract::Request::from_parts(parts, body))
                .await
        }
        Err(denied) => {
            let request_id = parts
                .extensions
                .get::<RequestId>()
                .map(|id| id.0.clone())
                .unwrap_or_default();
            server.log.info(
                "authorization_denied",
                &[
                    ("operation", operation.name().to_string()),
                    (
                        "principal",
                        principal
                            .as_ref()
                            .map_or("none", Principal::name)
                            .to_string(),
                    ),
                    (
                        "reason",
                        match denied {
                            Denied::NoPrincipal => "no_principal",
                            Denied::NotPermitted => "not_permitted",
                        }
                        .to_string(),
                    ),
                    ("request_id", request_id),
                ],
            );
            error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "this client may not perform this operation",
            )
        }
    }
}
