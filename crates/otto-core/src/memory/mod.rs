//! Service-neutral memory contracts. The host supplies authority, operation
//! control and transport; returned content and metadata never grant authority.
//! References bind to one logical service instance and admitted scope.
//! Implementations are concurrent, honor cancellation, and report uncertain
//! effects without inventing completion or safe retry guarantees.

mod types;
mod validate;

pub use types::*;
pub use validate::*;

use crate::operation::OperationControl;

/// Required recall port with explicitly optional management operations.
/// Descriptions declare support, not user authorization. Callers validate
/// requests and returned projections at their trust boundaries.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait MemoryService: Send + Sync {
    fn describe(&self) -> ServiceDescription;

    async fn recall(
        &self,
        request: &RecallRequest,
        control: &dyn OperationControl,
    ) -> Result<RecallResult>;

    async fn observe(
        &self,
        _request: &ObserveRequest,
        _control: &dyn OperationControl,
    ) -> Result<Receipt> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn remember(
        &self,
        _request: &RememberRequest,
        _control: &dyn OperationControl,
    ) -> Result<Receipt> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn reflect(
        &self,
        _request: &ReflectRequest,
        _control: &dyn OperationControl,
    ) -> Result<Receipt> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn forget(
        &self,
        _request: &ForgetRequest,
        _control: &dyn OperationControl,
    ) -> Result<Receipt> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn inspect(
        &self,
        _request: &InspectRequest,
        _control: &dyn OperationControl,
    ) -> Result<InspectResult> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn feedback(
        &self,
        _request: &FeedbackRequest,
        _control: &dyn OperationControl,
    ) -> Result<Receipt> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn prepare(
        &self,
        _request: &PrepareRequest,
        _control: &dyn OperationControl,
    ) -> Result<PreparedChanges> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn apply(
        &self,
        _request: &ApplyRequest,
        _control: &dyn OperationControl,
    ) -> Result<Receipt> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    async fn operation_status(
        &self,
        _request: &OperationStatusRequest,
        _control: &dyn OperationControl,
    ) -> Result<Receipt> {
        Err(Error::new(ErrorKind::Unsupported))
    }
    /// Releases local resources without deleting memory or asserting that
    /// accepted remote work has stopped. Closing must preserve pending handles.
    async fn close(&self, _control: &dyn OperationControl) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
