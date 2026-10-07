//! Fault decorator over [`Transport`].
//!
//! The plan subject of `Request` and `Notify` is the outbound method
//! (`tools/call`, `notifications/initialized`); `Close` carries the empty
//! subject. `timeout_stops_server` is forwarded to the inner transport.
//!
//! [`Fault::Before`] returns [`CallError::Transport`] carrying the injected
//! error's text without calling the inner transport. [`Fault::After`] calls
//! it first. [`Fault::Hang`] never returns and ignores the cancellation
//! token: a transport that does not honor cancellation is the fault, and the
//! client's call timeout and grace must end the call. `Exit`, `ExitAfter` and
//! the write faults panic: a plan that asks for them is a test bug.

use std::fmt::Debug;
use std::future::Future;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{Fault, FaultPlan};
use crate::mcp::{CallError, Outbound, Transport, jsonrpc};
use std::sync::Arc;

/// The transport operations a rule can select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpOp {
    Request,
    Notify,
    Close,
}

async fn run<T>(
    plan: &FaultPlan<McpOp>,
    op: McpOp,
    subject: &str,
    call: impl Future<Output = Result<T, CallError>>,
) -> Result<T, CallError> {
    match plan.check(op, subject) {
        None => call.await,
        Some(Fault::Before(error)) => Err(CallError::Transport(error.io().to_string())),
        Some(Fault::After(error)) => {
            call.await?;
            Err(CallError::Transport(error.io().to_string()))
        }
        Some(Fault::Hang) => std::future::pending().await,
        Some(fault) => panic!("{fault:?} is not supported on {op:?} for {subject:?}"),
    }
}

/// A [`Transport`] that applies a [`FaultPlan`] to the transport it wraps.
pub struct FaultyTransport {
    inner: Box<dyn Transport>,
    plan: Arc<FaultPlan<McpOp>>,
}

impl FaultyTransport {
    pub fn new(inner: Box<dyn Transport>, plan: Arc<FaultPlan<McpOp>>) -> Self {
        Self { inner, plan }
    }
}

#[async_trait::async_trait]
impl Transport for FaultyTransport {
    async fn request(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, jsonrpc::RpcError>, CallError> {
        let method = outbound.method.clone();
        run(
            &self.plan,
            McpOp::Request,
            &method,
            self.inner.request(outbound, cancel),
        )
        .await
    }

    async fn notify(&self, outbound: Outbound) -> Result<(), CallError> {
        let method = outbound.method.clone();
        run(
            &self.plan,
            McpOp::Notify,
            &method,
            self.inner.notify(outbound),
        )
        .await
    }

    fn timeout_stops_server(&self) -> bool {
        self.inner.timeout_stops_server()
    }

    async fn close(&self) {
        // `close` cannot fail, so an injected error leaves the inner
        // transport open.
        let _ = run(&self.plan, McpOp::Close, "", async {
            self.inner.close().await;
            Ok(())
        })
        .await;
    }
}
