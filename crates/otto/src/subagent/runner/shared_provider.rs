//! The provider handle shared between the parent and its children.

use super::*;

/// Shares one provider between the parent and every child, because [`Agent`]
/// owns the provider it calls.
pub(crate) struct SharedProvider {
    pub(super) provider: Arc<dyn Provider + Send + Sync>,
    pub(super) timeout: Option<Duration>,
    pub(super) cancellation_grace: Duration,
}

#[async_trait::async_trait]
impl Provider for SharedProvider {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.complete_observed(request, emit, control, &otto_core::diagnostics::Noop)
            .await
    }

    fn complete_observed<'a>(
        &'a self,
        request: &'a Request,
        emit: StreamSink<'a>,
        control: &'a dyn OperationControl,
        observer: &'a dyn otto_core::diagnostics::Observer,
    ) -> otto_core::provider::DiagnosticFuture<'a> {
        Box::pin(async move {
            if let Some(reason) = control.admission_stop_reason() {
                return ProviderSettlement::stopped(
                    if reason == OperationStopReason::Deadline {
                        ProviderError::DeadlineExceeded
                    } else {
                        ProviderError::Cancelled
                    },
                    0,
                    otto_core::model::EffectCertainty::NotStarted,
                    reason,
                );
            }
            let child = Control::new(Deadline::child(control.remaining(), self.timeout));
            let mut complete = std::pin::pin!(
                self.provider
                    .complete_observed(request, emit, &child, observer)
            );
            let deadline = child.deadline();
            let reason = tokio::select! {
                biased;
                result = &mut complete => return result,
                () = control.cancellation_token().cancelled() => {
                    let reason = control
                        .stop_reason()
                        .unwrap_or(OperationStopReason::UserCancellation);
                    child.stop(reason);
                    reason
                }
                () = deadline.expired() => {
                    child.stop(OperationStopReason::Deadline);
                    OperationStopReason::Deadline
                }
            };
            match tokio::time::timeout(self.cancellation_grace, &mut complete).await {
                Ok(settlement) => settlement,
                Err(_) => ProviderSettlement::stopped(
                    if reason == OperationStopReason::Deadline {
                        ProviderError::DeadlineExceeded
                    } else {
                        ProviderError::Cancelled
                    },
                    1,
                    otto_core::model::EffectCertainty::Unknown,
                    reason,
                ),
            }
        })
    }
}
