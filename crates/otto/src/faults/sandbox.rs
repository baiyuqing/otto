//! Fault decorator over the sandbox [`Driver`].
//!
//! The plan subject of `Execute` is the request's program, `argv[0]`.
//! `execute` and `execute_with_grace` both count as `Execute`.
//!
//! Sandbox errors are not I/O errors, so [`Fault::Before`] always reports
//! [`Error::ChildLaunch`] with a default [`ExitStatus`], whatever error the
//! rule carries, and does not call the inner driver. Every other fault kind
//! panics: a plan that asks for it is a test bug.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{Fault, FaultPlan};
use crate::sandbox::{Capabilities, Driver, DriverId, Error, ExitStatus, Request, Streams};

/// The sandbox operations a rule can select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxOp {
    Execute,
}

/// A [`Driver`] that applies a [`FaultPlan`] to the driver it wraps.
pub struct FaultyDriver {
    inner: Arc<dyn Driver>,
    plan: Arc<FaultPlan<SandboxOp>>,
}

impl FaultyDriver {
    pub fn new(inner: Arc<dyn Driver>, plan: Arc<FaultPlan<SandboxOp>>) -> Self {
        Self { inner, plan }
    }

    /// `Some` when the plan fails this execution before the inner driver runs.
    fn check(&self, request: &Request) -> Option<(ExitStatus, Result<(), Error>)> {
        let program = request.argv.first().map_or("", String::as_str);
        match self.plan.check(SandboxOp::Execute, program)? {
            Fault::Before(_) => Some((ExitStatus::default(), Err(Error::ChildLaunch))),
            fault => panic!("{fault:?} is not supported on Execute for {program:?}"),
        }
    }
}

#[async_trait]
impl Driver for FaultyDriver {
    fn id(&self) -> DriverId {
        self.inner.id()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        if let Some(failed) = self.check(&request) {
            return failed;
        }
        self.inner.execute(request, streams, cancel).await
    }

    async fn execute_with_grace(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
        cancellation_grace: Duration,
    ) -> (ExitStatus, Result<(), Error>) {
        if let Some(failed) = self.check(&request) {
            return failed;
        }
        self.inner
            .execute_with_grace(request, streams, cancel, cancellation_grace)
            .await
    }

    fn close(&self) -> Result<(), Error> {
        self.inner.close()
    }
}
