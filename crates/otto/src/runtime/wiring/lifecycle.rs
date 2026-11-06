//! The lifecycle query of the agent seam over the neutral memory port: one
//! `inspect` Get per id and scope, mapped to a record status.

use otto_core::agent::memory::{MemoryError, RecordLifecycle, RecordStatus};
use otto_core::memory as port;

use super::ServiceBinding;

/// Metadata keys the built-in adapter exports with each item.
const STATE_KEY: &str = "otto.state";
const REPLACEMENT_KEY: &str = "otto.superseded_by";

impl ServiceBinding {
    /// The status of each id in the first scope of the context that holds it.
    /// A service that reports the id as absent (`Conflict` for a Get without
    /// a revision), or returns no item, yields `Unknown`: a port service
    /// cannot distinguish a forgotten record from an absent one. A service
    /// that does not support `inspect` yields `Unknown` for every id.
    pub(super) async fn record_lifecycles(
        &self,
        ids: &[String],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<RecordLifecycle>, MemoryError> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(MemoryError(
                port::Error::new(port::ErrorKind::Closed).to_string(),
            ));
        }
        let mut lifecycles = Vec::with_capacity(ids.len());
        for id in ids {
            lifecycles.push(self.record_lifecycle(id, cancel).await?);
        }
        Ok(lifecycles)
    }

    async fn record_lifecycle(
        &self,
        id: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<RecordLifecycle, MemoryError> {
        let service = self.client.describe().identity.clone();
        for scope in &self.context.scopes {
            let request = port::InspectRequest {
                context: self.context.clone(),
                query: port::InspectQuery::Get(port::MemoryRef {
                    organization_id: self.context.organization_id.clone(),
                    service: service.clone(),
                    scope: scope.clone(),
                    id: id.to_string(),
                    revision: None,
                }),
            };
            let result = match self.client.inspect(&request, cancel).await {
                Ok(result) => result,
                Err(error) if error.kind == port::ErrorKind::Conflict => continue,
                // A service without the Inspect operation has no lifecycle
                // data: answer Unknown instead of a warning on every step.
                Err(error) if error.kind == port::ErrorKind::Unsupported => break,
                Err(error) => return Err(MemoryError(error.to_string())),
            };
            let Some(item) = result.items.into_iter().next() else {
                continue;
            };
            let Some(reference) = item.reference else {
                continue;
            };
            let replacement_id = item.metadata.get(REPLACEMENT_KEY).cloned();
            let status = match item.metadata.get(STATE_KEY).map(String::as_str) {
                None | Some("active") => RecordStatus::Active,
                Some("needs_review") => RecordStatus::NeedsReview,
                Some("superseded") => RecordStatus::Superseded {
                    replacement_id: replacement_id.unwrap_or_default(),
                },
                Some("retracted") => RecordStatus::Retracted,
                Some(_) => RecordStatus::Unknown,
            };
            return Ok(RecordLifecycle {
                id: id.to_string(),
                revision: reference.revision,
                status,
            });
        }
        Ok(RecordLifecycle {
            id: id.to_string(),
            revision: None,
            status: RecordStatus::Unknown,
        })
    }
}
