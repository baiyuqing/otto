//! Provider client dispatch, request timeouts and observed calls.

use super::*;

#[async_trait::async_trait]
impl Provider for ProviderClient {
    fn capabilities(&self) -> otto_core::provider::ProviderCapabilities {
        match self {
            Self::Compat { client, .. } => client.capabilities(),
            Self::ChatGpt { client, .. } => client.capabilities(),
            Self::Unavailable => otto_core::provider::ProviderCapabilities::default(),
            #[cfg(test)]
            Self::Scripted(provider) => provider.capabilities(),
        }
    }

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
            match self {
                Self::Compat {
                    client,
                    timeout,
                    cancellation_grace,
                } => {
                    complete_with_timeout(
                        &ObservedProvider {
                            inner: client.as_ref(),
                            observer,
                        },
                        request,
                        emit,
                        control,
                        *timeout,
                        *cancellation_grace,
                    )
                    .await
                }
                Self::ChatGpt {
                    client,
                    timeout,
                    cancellation_grace,
                } => {
                    complete_with_timeout(
                        &ObservedProvider {
                            inner: client.as_ref(),
                            observer,
                        },
                        request,
                        emit,
                        control,
                        *timeout,
                        *cancellation_grace,
                    )
                    .await
                }
                Self::Unavailable => ProviderSettlement::failed(
                    ProviderError::Other(
                        "provider is unavailable: redaction is incomplete".to_string(),
                    ),
                    0,
                    otto_core::model::EffectCertainty::NotStarted,
                ),
                #[cfg(test)]
                Self::Scripted(provider) => {
                    provider
                        .complete_observed(request, emit, control, observer)
                        .await
                }
            }
        })
    }
}

struct ObservedProvider<'a, P: ?Sized> {
    inner: &'a P,
    observer: &'a dyn otto_core::diagnostics::Observer,
}
#[async_trait::async_trait]
impl<P: Provider + Sync + ?Sized> Provider for ObservedProvider<'_, P> {
    fn capabilities(&self) -> otto_core::provider::ProviderCapabilities {
        self.inner.capabilities()
    }
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.inner
            .complete_observed(request, emit, control, self.observer)
            .await
    }
}

pub(super) fn provider_stop_settlement(reason: OperationStopReason) -> ProviderSettlement {
    ProviderSettlement::stopped(
        if reason == OperationStopReason::Deadline {
            ProviderError::DeadlineExceeded
        } else {
            ProviderError::Cancelled
        },
        0,
        otto_core::model::EffectCertainty::NotStarted,
        reason,
    )
}

pub(super) async fn complete_with_timeout<P: Provider + ?Sized>(
    provider: &P,
    request: &Request,
    emit: StreamSink<'_>,
    parent: &dyn OperationControl,
    timeout: Option<Duration>,
    cancellation_grace: Duration,
) -> ProviderSettlement {
    if let Some(reason) = parent.admission_stop_reason() {
        return provider_stop_settlement(reason);
    }
    let control = Control::new(Deadline::child(parent.remaining(), timeout));
    let mut complete = std::pin::pin!(provider.complete(request, emit, &control));
    let deadline = control.deadline();
    let reason = tokio::select! {
        biased;
        result = &mut complete => return result,
        () = parent.cancellation_token().cancelled() => {
            let reason = parent
                .stop_reason()
                .unwrap_or(OperationStopReason::UserCancellation);
            control.stop(reason);
            reason
        }
        () = deadline.expired() => {
            control.stop(OperationStopReason::Deadline);
            OperationStopReason::Deadline
        }
    };
    match tokio::time::timeout(cancellation_grace, &mut complete).await {
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
}

pub(super) async fn drive_with_control<F, T>(
    future: F,
    parent_cancel: &CancellationToken,
    control: &Control,
) -> T
where
    F: Future<Output = T>,
{
    let mut future = std::pin::pin!(future);
    let deadline = control.deadline();
    tokio::select! {
        biased;
        result = &mut future => result,
        () = parent_cancel.cancelled() => {
            control.stop(OperationStopReason::UserCancellation);
            future.await
        }
        () = deadline.expired() => {
            control.stop(OperationStopReason::Deadline);
            future.await
        }
    }
}

impl Builder {
    /// The provider client and the secret redactor the normal agents use for `runtime`. The
    /// development evaluation (`otto eval view-tree`) calls its provider only through an
    /// `Agent` built with this redactor.
    pub(crate) fn eval_provider_access(
        &self,
        runtime: &Runtime,
    ) -> Result<(ProviderClient, Redactor), BuildError> {
        Ok((
            self.provider_client(runtime)?,
            self.boundary_redactor(Some(runtime)),
        ))
    }
}
