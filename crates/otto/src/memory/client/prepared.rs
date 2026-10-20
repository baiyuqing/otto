//! Prepared changes: prepare, restore, apply and operation status.

use super::*;

impl Client {
    pub async fn prepare(
        &self,
        request: &PrepareRequest,
        control: &dyn OperationControl,
    ) -> Result<PreparedChanges> {
        request.validate(&self.description.identity).map_err(safe)?;
        let capability = self.admit(Operation::Prepare, control)?;
        if !capability.supports_reviewed_changes
            || capability
                .effects
                .iter()
                .any(|effect| *effect != Effect::StoreEvidence)
        {
            return Err(err(ErrorKind::Unsupported));
        }
        if let MutatingRequest::Forget(forget) = &request.operation {
            let capability = self
                .description
                .capability(Operation::Forget)
                .map_err(safe)?;
            if !capability.forget_targets.contains(&forget.target.kind())
                || !capability.forget_levels.contains(&forget.coverage)
            {
                return Err(err(ErrorKind::Unsupported));
            }
        }
        if self
            .prepared
            .lock()
            .map_err(|_| err(ErrorKind::Unavailable))?
            .len()
            >= MAX_PREPARED
        {
            return Err(err(ErrorKind::Rejected));
        }
        self.guarded(request)?;
        let result = self.service.prepare(request, control).await.map_err(safe)?;
        result
            .validate(request, &self.description.identity)
            .map_err(safe)?;
        if result.effects.contains(&Effect::BackgroundEvolution) {
            return Err(err(ErrorKind::Unsupported));
        }
        self.guarded(&result)?;
        self.retain_prepared(result.clone(), false)?;
        Ok(result)
    }

    pub(super) fn retain_prepared(&self, snapshot: PreparedChanges, attempted: bool) -> Result<()> {
        let mut prepared = self
            .prepared
            .lock()
            .map_err(|_| err(ErrorKind::Unavailable))?;
        if let Some(existing) = prepared.get_mut(&snapshot.reference) {
            if existing.snapshot != snapshot {
                return Err(err(ErrorKind::Conflict));
            }
            existing.attempted |= attempted;
            return Ok(());
        } else if prepared.len() >= MAX_PREPARED {
            return Err(err(ErrorKind::Rejected));
        }
        prepared.insert(
            snapshot.reference.clone(),
            PreparedEntry {
                snapshot,
                attempted,
            },
        );
        Ok(())
    }

    /// Restore a subject verified by the trusted Rust host's durable approval
    /// domain. The supplied context must come from an authenticated human
    /// entry point. A digest proves equality, not that a human approved it;
    /// callers must verify their recorded approval before invoking this method.
    /// This host API must never be exposed as a model-callable operation.
    pub fn restore_prepared(
        &self,
        snapshot: PreparedChanges,
        approved_context: &MemoryContext,
        approved_digest: &str,
        recovery: PreparationRecovery,
    ) -> Result<()> {
        if self.is_closed() {
            return Err(err(ErrorKind::Closed));
        }
        if approved_context.origin != Origin::Human {
            return Err(err(ErrorKind::Unauthorized));
        }
        snapshot
            .validate_snapshot(approved_context, &self.description.identity)
            .map_err(safe)?;
        if prepared_digest(&snapshot)? != approved_digest {
            return Err(err(ErrorKind::Conflict));
        }
        self.guarded(&snapshot)?;
        self.retain_prepared(snapshot, recovery == PreparationRecovery::Attempted)
    }

    pub async fn apply(
        &self,
        snapshot: &PreparedChanges,
        request: &ApplyRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate().map_err(safe)?;
        snapshot
            .validate_snapshot(&request.context, &self.description.identity)
            .map_err(safe)?;
        let capability = self.admit(Operation::Apply, control)?;
        if request.context.origin != Origin::Human
            || !capability.supports_reviewed_changes
            || capability.effects.contains(&Effect::BackgroundEvolution)
            || snapshot.effects.contains(&Effect::BackgroundEvolution)
        {
            return Err(err(ErrorKind::Unauthorized));
        }
        if snapshot.reference != request.preparation
            || snapshot.context.principal != request.context.principal
            || snapshot
                .expires_at
                .is_some_and(|expiry| chrono::Utc::now().timestamp() >= expiry)
        {
            return Err(err(ErrorKind::Conflict));
        }
        if snapshot
            .effects
            .iter()
            .any(|effect| !capability.effects.contains(effect))
        {
            return Err(err(ErrorKind::Unsupported));
        }
        if self.mode == AuthorizationMode::DelegatedLearning {
            let effective = self.effective_context(
                &request.context,
                snapshot
                    .changes
                    .iter()
                    .map(|change| change.scope.clone())
                    .collect(),
            )?;
            self.grant(&effective, &["explicit_memory"], capability)?;
        }
        let mut effective = request.clone();
        effective.context = self.effective_context(
            &request.context,
            snapshot
                .changes
                .iter()
                .map(|change| change.scope.clone())
                .collect(),
        )?;
        let request = &effective;
        self.guarded(snapshot)?;
        self.guarded(request)?;
        self.admit(Operation::Apply, control)?;
        {
            let mut prepared = self
                .prepared
                .lock()
                .map_err(|_| err(ErrorKind::Unavailable))?;
            let entry = prepared
                .get_mut(&snapshot.reference)
                .ok_or_else(|| err(ErrorKind::Conflict))?;
            if entry.snapshot != *snapshot || entry.attempted {
                return Err(err(ErrorKind::Conflict));
            }
            entry.attempted = true;
        }
        let receipt = self.checked_receipt(
            &request.context,
            &request.request_id,
            self.service.apply(request, control).await.map_err(safe)?,
        )?;
        // Keep pending/unknown preparations recoverable. A completed receipt
        // does not authorize replay of the consumed preparation.
        if receipt.completion == Completion::Complete {
            self.prepared
                .lock()
                .map_err(|_| err(ErrorKind::Unavailable))?
                .remove(&snapshot.reference);
        }
        Ok(receipt)
    }

    pub async fn operation_status(
        &self,
        request: &OperationStatusRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate().map_err(safe)?;
        self.admit(Operation::OperationStatus, control)?;
        self.guarded(request)?;
        let receipt = self
            .service
            .operation_status(request, control)
            .await
            .map_err(safe)?;
        if receipt.operation_ref.as_ref() != Some(&request.operation_ref) {
            return Err(err(ErrorKind::Conflict));
        }
        if self
            .correlations
            .lock()
            .map_err(|_| err(ErrorKind::Unavailable))?
            .get(&request.operation_ref)
            .is_some_and(|known| known != &receipt.request_id)
        {
            return Err(err(ErrorKind::Conflict));
        }
        // Status has no caller request ID; validate the returned ID without
        // inventing correlation or changing completion/visibility.
        receipt
            .validate(
                &request.context,
                &receipt.request_id,
                &self.description.identity,
            )
            .map_err(safe)?;
        self.guarded(&receipt)?;
        Ok(receipt)
    }
}
