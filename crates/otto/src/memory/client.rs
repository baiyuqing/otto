//! Host admission for a selected memory service. Service descriptions and
//! returned metadata never grant authority. Reviewed snapshots are retained
//! locally; durable approval recovery must restore them through the trusted
//! host-only recovery entry point before application.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use otto_core::{memory::*, operation::OperationControl};
use sha2::{Digest, Sha256};

use super::{
    GuardField, GuardInput,
    guard::{ContentGuard, DefaultGuard},
};

const MAX_PREPARED: usize = 64;

/// Durable application state established by the host approval domain.
/// Recovery defaults to refusing another submission. `NotStarted` requires
/// positive evidence that the approved operation was never submitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PreparationRecovery {
    NotStarted,
    #[default]
    Attempted,
}

struct PreparedEntry {
    snapshot: PreparedChanges,
    attempted: bool,
}

pub struct Client {
    service: Arc<dyn MemoryService>,
    description: ServiceDescription,
    mode: AuthorizationMode,
    grant: Option<LearningGrant>,
    guard: Arc<dyn ContentGuard>,
    prepared: Mutex<BTreeMap<String, PreparedEntry>>,
    correlations: Mutex<BTreeMap<String, String>>,
    provider_shutdown: bool,
    closed: AtomicBool,
    close_result: tokio::sync::OnceCell<Result<()>>,
}

impl Client {
    pub fn new(service: Arc<dyn MemoryService>) -> Result<Self> {
        let description = service.describe();
        description.validate().map_err(safe)?;
        let client = Self {
            service,
            description,
            mode: AuthorizationMode::ReviewedChanges,
            grant: None,
            guard: Arc::new(DefaultGuard),
            prepared: Mutex::new(BTreeMap::new()),
            correlations: Mutex::new(BTreeMap::new()),
            provider_shutdown: false,
            closed: AtomicBool::new(false),
            close_result: tokio::sync::OnceCell::new(),
        };
        client.guarded(&client.description)?;
        Ok(client)
    }

    /// Explicit host-owned authority. A remote description cannot create it.
    pub fn with_learning(service: Arc<dyn MemoryService>, grant: LearningGrant) -> Result<Self> {
        let mut client = Self::new(service)?;
        let context = MemoryContext {
            principal: "host-grant-validation".into(),
            scopes: grant.scopes.clone(),
            write_scope: None,
            workspace_id: None,
            session_id: None,
            task_id: None,
            origin: Origin::Human,
        };
        grant.validate(&context).map_err(safe)?;
        if grant.input_classes.iter().any(|class| {
            !matches!(
                class.as_str(),
                "message" | "tool_result" | "task" | "explicit_memory" | "reflection" | "feedback"
            )
        }) {
            return Err(err(ErrorKind::InvalidInput));
        }
        client.mode = AuthorizationMode::DelegatedLearning;
        client.grant = Some(grant);
        Ok(client)
    }

    pub fn with_guard(mut self, guard: Arc<dyn ContentGuard>) -> Result<Self> {
        self.guard = guard;
        self.guarded(&self.description)?;
        Ok(self)
    }
    /// A client over the same service and guard with another learning grant.
    /// It does not own provider shutdown.
    pub(crate) fn with_grant(&self, grant: LearningGrant) -> Result<Self> {
        Self::with_learning(self.service.clone(), grant)?.with_guard(self.guard.clone())
    }

    /// Transfer provider shutdown responsibility to this client explicitly.
    /// The host must assign a single owner for a shared provider instance.
    pub fn with_provider_shutdown(mut self, owned: bool) -> Self {
        self.provider_shutdown = owned;
        self
    }
    pub fn describe(&self) -> &ServiceDescription {
        &self.description
    }
    pub fn mode(&self) -> AuthorizationMode {
        self.mode
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn admit(&self, operation: Operation, control: &dyn OperationControl) -> Result<&Capability> {
        if self.is_closed() {
            return Err(err(ErrorKind::Closed));
        }
        if control.admission_stop_reason().is_some()
            || control.cancellation_token().is_cancelled()
            || control
                .remaining()
                .is_some_and(|remaining| remaining.is_zero())
        {
            return Err(err(ErrorKind::Canceled));
        }
        self.description.capability(operation).map_err(safe)
    }

    fn guarded(&self, value: &impl serde::Serialize) -> Result<()> {
        let value = serde_json::to_value(value).map_err(|_| err(ErrorKind::InvalidInput))?;
        let mut fields = Vec::new();
        guard_fields(&value, "memory", &mut fields);
        let mut chunk = Vec::new();
        let mut bytes = 0;
        for field in fields {
            if field.value.len() > super::MAX_GUARD_BYTES {
                return Err(err(ErrorKind::Rejected));
            }
            if chunk.len() == super::MAX_GUARD_FIELDS
                || bytes + field.value.len() > super::MAX_GUARD_BYTES
            {
                self.guard
                    .check(&GuardInput {
                        fields: std::mem::take(&mut chunk),
                    })
                    .map_err(|_| err(ErrorKind::Rejected))?;
                bytes = 0;
            }
            bytes += field.value.len();
            chunk.push(field);
        }
        self.guard
            .check(&GuardInput { fields: chunk })
            .map_err(|_| err(ErrorKind::Rejected))
    }

    fn grant(
        &self,
        context: &MemoryContext,
        classes: &[&str],
        capability: &Capability,
    ) -> Result<()> {
        let grant = self
            .grant
            .as_ref()
            .ok_or_else(|| err(ErrorKind::Unauthorized))?;
        if !capability.supports_delegated_learning
            || (capability.effects.contains(&Effect::BackgroundEvolution) && !grant.background)
            || context
                .scopes
                .iter()
                .any(|scope| !grant.scopes.contains(scope))
            || classes
                .iter()
                .any(|class| !grant.input_classes.iter().any(|allowed| allowed == class))
        {
            return Err(err(ErrorKind::Unauthorized));
        }
        Ok(())
    }

    fn direct(&self, context: &MemoryContext, class: &str, capability: &Capability) -> Result<()> {
        match self.mode {
            AuthorizationMode::ReviewedChanges => {
                if context.origin != Origin::Human
                    || !capability.supports_reviewed_changes
                    || capability.effects.contains(&Effect::BackgroundEvolution)
                {
                    return Err(err(ErrorKind::Unauthorized));
                }
                Ok(())
            }
            AuthorizationMode::DelegatedLearning => self.grant(context, &[class], capability),
        }
    }

    fn evidence_scope(&self, context: &MemoryContext) -> Result<()> {
        if let Some(grant) = &self.grant
            && context
                .scopes
                .iter()
                .any(|scope| !grant.scopes.contains(scope))
        {
            return Err(err(ErrorKind::Unauthorized));
        }
        Ok(())
    }

    fn effective_context(
        &self,
        context: &MemoryContext,
        scopes: Vec<Scope>,
    ) -> Result<MemoryContext> {
        if self.mode == AuthorizationMode::ReviewedChanges {
            return Ok(context.clone());
        }
        let mut effective = context.clone();
        effective.scopes.clear();
        for scope in scopes {
            context.validate_scope(&scope).map_err(safe)?;
            if !effective.scopes.contains(&scope) {
                effective.scopes.push(scope);
            }
        }
        if effective.scopes.is_empty() {
            return Err(err(ErrorKind::Unauthorized));
        }
        effective.write_scope = context
            .write_scope
            .clone()
            .filter(|scope| effective.scopes.contains(scope));
        effective.validate().map_err(safe)?;
        self.evidence_scope(&effective)?;
        Ok(effective)
    }

    fn write_scope(context: &MemoryContext) -> Result<Scope> {
        context
            .write_scope
            .clone()
            .ok_or_else(|| err(ErrorKind::InvalidInput))
    }

    fn checked_receipt(
        &self,
        context: &MemoryContext,
        request_id: &str,
        receipt: Receipt,
    ) -> Result<Receipt> {
        receipt
            .validate(context, request_id, &self.description.identity)
            .map_err(safe)?;
        self.guarded(&receipt)?;
        if let Some(operation_ref) = &receipt.operation_ref {
            let mut correlations = self
                .correlations
                .lock()
                .map_err(|_| err(ErrorKind::Unavailable))?;
            if correlations
                .get(operation_ref)
                .is_some_and(|known| known != request_id)
            {
                return Err(err(ErrorKind::Conflict));
            }
            if correlations.len() == MAX_PREPARED && !correlations.contains_key(operation_ref) {
                // Bounded correlation is only a checking aid, never authority
                // or a promise that historical operation status is available.
                correlations.pop_first();
            }
            correlations.insert(operation_ref.clone(), request_id.into());
        }
        Ok(receipt)
    }

    pub async fn recall(
        &self,
        request: &RecallRequest,
        control: &dyn OperationControl,
    ) -> Result<RecallResult> {
        request.validate().map_err(safe)?;
        self.admit(Operation::Recall, control)?;
        self.guarded(request)?;
        let result = self.service.recall(request, control).await.map_err(safe)?;
        result
            .validate(request, &self.description.identity)
            .map_err(safe)?;
        self.guarded(&result)?;
        Ok(result)
    }

    pub async fn inspect(
        &self,
        request: &InspectRequest,
        control: &dyn OperationControl,
    ) -> Result<InspectResult> {
        request.validate(&self.description.identity).map_err(safe)?;
        self.admit(Operation::Inspect, control)?;
        self.guarded(request)?;
        let result = self.service.inspect(request, control).await.map_err(safe)?;
        result
            .validate(request, &self.description.identity)
            .map_err(safe)?;
        self.guarded(&result)?;
        Ok(result)
    }

    pub async fn remember(
        &self,
        request: &RememberRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate(&self.description.identity).map_err(safe)?;
        let capability = self.admit(Operation::Remember, control)?;
        let mut effective = request.clone();
        let scope = match &request.target {
            Some(target) => target.scope.clone(),
            None => Self::write_scope(&request.context)?,
        };
        effective.context = self.effective_context(&request.context, vec![scope])?;
        let request = &effective;
        self.direct(&request.context, "explicit_memory", capability)?;
        self.guarded(request)?;
        self.checked_receipt(
            &request.context,
            &request.request_id,
            self.service
                .remember(request, control)
                .await
                .map_err(safe)?,
        )
    }

    pub async fn forget(
        &self,
        request: &ForgetRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate(&self.description.identity).map_err(safe)?;
        let capability = self.admit(Operation::Forget, control)?;
        if !capability.forget_targets.contains(&request.target.kind())
            || !capability.forget_levels.contains(&request.coverage)
        {
            return Err(err(ErrorKind::Unsupported));
        }
        let scopes = match &request.target {
            ForgetTarget::Item(target) => vec![target.scope.clone()],
            ForgetTarget::Scope(scope) => vec![scope.clone()],
            ForgetTarget::Source(_) => request.context.scopes.clone(),
        };
        let mut effective = request.clone();
        effective.context = self.effective_context(&request.context, scopes)?;
        let request = &effective;
        self.direct(&request.context, "explicit_memory", capability)?;
        self.guarded(request)?;
        let receipt = self.service.forget(request, control).await.map_err(safe)?;
        receipt
            .validate_forget(request, &self.description.identity)
            .map_err(safe)?;
        self.checked_receipt(&request.context, &request.request_id, receipt)
    }

    pub async fn observe(
        &self,
        request: &ObserveRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate().map_err(safe)?;
        let capability = self.admit(Operation::Observe, control)?;
        let mut effective = request.clone();
        effective.context =
            self.effective_context(&request.context, vec![Self::write_scope(&request.context)?])?;
        let request = &effective;
        match request.mode {
            ObserveMode::ArchiveOnly => {
                self.evidence_scope(&request.context)?;
                if capability.effects != [Effect::StoreEvidence] {
                    return Err(err(ErrorKind::Unsupported));
                }
            }
            ObserveMode::Learn => {
                let classes: Vec<_> = request
                    .observations
                    .iter()
                    .map(|observation| input_class(&observation.source))
                    .collect::<Result<_>>()?;
                self.grant(&request.context, &classes, capability)?;
            }
        }
        self.guarded(request)?;
        self.checked_receipt(
            &request.context,
            &request.request_id,
            self.service.observe(request, control).await.map_err(safe)?,
        )
    }

    pub async fn reflect(
        &self,
        request: &ReflectRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate().map_err(safe)?;
        let capability = self.admit(Operation::Reflect, control)?;
        let scopes = if request.evidence.is_empty() {
            request.context.scopes.clone()
        } else {
            vec![Self::write_scope(&request.context)?]
        };
        let mut effective = request.clone();
        effective.context = self.effective_context(&request.context, scopes)?;
        let request = &effective;
        let mut classes = vec!["reflection"];
        classes.extend(
            request
                .evidence
                .iter()
                .map(|observation| input_class(&observation.source))
                .collect::<Result<Vec<_>>>()?,
        );
        classes.extend(
            request
                .sources
                .iter()
                .map(input_class)
                .collect::<Result<Vec<_>>>()?,
        );
        self.grant(&request.context, &classes, capability)?;
        self.guarded(request)?;
        self.checked_receipt(
            &request.context,
            &request.request_id,
            self.service.reflect(request, control).await.map_err(safe)?,
        )
    }

    pub async fn feedback(
        &self,
        request: &FeedbackRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate(&self.description.identity).map_err(safe)?;
        let capability = self.admit(Operation::Feedback, control)?;
        let mut effective = request.clone();
        effective.context = self.effective_context(
            &request.context,
            request
                .references
                .iter()
                .map(|reference| reference.scope.clone())
                .collect(),
        )?;
        let request = &effective;
        if capability
            .effects
            .iter()
            .any(|effect| *effect != Effect::StoreEvidence)
        {
            self.direct(&request.context, "feedback", capability)?;
        } else {
            self.evidence_scope(&request.context)?;
        }
        self.guarded(request)?;
        self.checked_receipt(
            &request.context,
            &request.request_id,
            self.service
                .feedback(request, control)
                .await
                .map_err(safe)?,
        )
    }

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

    fn retain_prepared(&self, snapshot: PreparedChanges, attempted: bool) -> Result<()> {
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

    pub async fn close(&self, control: &dyn OperationControl) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        self.close_result
            .get_or_init(|| async {
                if control.admission_stop_reason().is_some()
                    || control.cancellation_token().is_cancelled()
                    || control
                        .remaining()
                        .is_some_and(|remaining| remaining.is_zero())
                {
                    return Err(err(ErrorKind::Canceled));
                }
                if self.provider_shutdown {
                    self.service.close(control).await.map_err(safe)
                } else {
                    Ok(())
                }
            })
            .await
            .clone()
    }
}

/// Canonical snapshot equality digest for the host's durable approval subject.
/// Possessing this value does not grant permission to apply the snapshot.
pub fn prepared_digest(snapshot: &PreparedChanges) -> Result<String> {
    let bytes = serde_json::to_vec(snapshot).map_err(|_| err(ErrorKind::InvalidInput))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn err(kind: ErrorKind) -> Error {
    Error::new(kind)
}
fn safe(error: Error) -> Error {
    Error::new(error.kind)
}
fn input_class(source: &SourceRef) -> Result<&'static str> {
    match source.role.as_str() {
        "user" | "assistant" | "system" | "message" => Ok("message"),
        "tool" | "tool_result" => Ok("tool_result"),
        "task" => Ok("task"),
        _ => Err(err(ErrorKind::Unauthorized)),
    }
}
fn guard_fields(value: &serde_json::Value, name: &str, fields: &mut Vec<GuardField>) {
    match value {
        serde_json::Value::String(value) => fields.push(GuardField {
            name: name.into(),
            value: value.clone(),
            opaque: false,
        }),
        serde_json::Value::Array(values) => {
            for value in values {
                guard_fields(value, name, fields);
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                if name == "metadata" {
                    fields.push(GuardField {
                        name: "metadata key".into(),
                        value: key.clone(),
                        opaque: false,
                    });
                }
                guard_fields(value, key, fields);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio_util::sync::CancellationToken;

    struct Fake {
        description: ServiceDescription,
        calls: AtomicUsize,
        close_calls: AtomicUsize,
        recall: Option<RecallResult>,
        completion: Completion,
        fail_close: bool,
        bad_receipt: bool,
        remember_reference: Option<MemoryRef>,
        remember_contexts: Mutex<Vec<MemoryContext>>,
    }
    impl Fake {
        fn new(operations: &[Operation]) -> Self {
            let mut capabilities = vec![Capability::new(Operation::Recall)];
            capabilities.extend(
                operations
                    .iter()
                    .copied()
                    .filter(|operation| *operation != Operation::Recall)
                    .map(|operation| {
                        let mut capability = Capability::new(operation);
                        capability.supports_reviewed_changes = true;
                        capability.supports_delegated_learning = true;
                        if matches!(operation, Operation::Remember | Operation::Apply) {
                            capability.effects = vec![Effect::ChangeMemory];
                        }
                        capability
                    }),
            );
            Self {
                description: ServiceDescription {
                    identity: "fake-service".into(),
                    encryption_at_rest: None,
                    version: 1,
                    capabilities,
                },
                calls: AtomicUsize::new(0),
                close_calls: AtomicUsize::new(0),
                recall: None,
                completion: Completion::Complete,
                fail_close: false,
                bad_receipt: false,
                remember_reference: None,
                remember_contexts: Mutex::new(vec![]),
            }
        }
        fn receipt(&self, request_id: &str) -> Receipt {
            Receipt {
                request_id: request_id.into(),
                operation_ref: None,
                admission: Admission::Acknowledged,
                completion: self.completion,
                visibility: Visibility::Unknown,
                items: if self.bad_receipt {
                    vec![ItemOutcome {
                        reference: None,
                        state: ItemState::Unknown,
                        reason: None,
                    }]
                } else if let Some(reference) = &self.remember_reference {
                    vec![ItemOutcome {
                        reference: Some(reference.clone()),
                        state: ItemState::Applied,
                        reason: None,
                    }]
                } else {
                    vec![]
                },
                forget_coverage: None,
            }
        }
    }
    #[async_trait::async_trait]
    impl MemoryService for Fake {
        fn describe(&self) -> ServiceDescription {
            self.description.clone()
        }
        async fn recall(
            &self,
            _: &RecallRequest,
            _: &dyn OperationControl,
        ) -> Result<RecallResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.recall.clone().unwrap_or(RecallResult {
                items: vec![],
                used_tokens: Some(0),
                coverage: Coverage::Unknown,
            }))
        }
        async fn remember(
            &self,
            request: &RememberRequest,
            _: &dyn OperationControl,
        ) -> Result<Receipt> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.remember_contexts
                .lock()
                .unwrap()
                .push(request.context.clone());
            Ok(self.receipt(&request.request_id))
        }
        async fn observe(
            &self,
            request: &ObserveRequest,
            _: &dyn OperationControl,
        ) -> Result<Receipt> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.receipt(&request.request_id))
        }
        async fn prepare(
            &self,
            request: &PrepareRequest,
            _: &dyn OperationControl,
        ) -> Result<PreparedChanges> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let MutatingRequest::Remember(remember) = &request.operation else {
                return Err(err(ErrorKind::Unsupported));
            };
            Ok(PreparedChanges {
                reference: "prepared-1".into(),
                context: remember.context.clone(),
                changes: vec![Change {
                    action: ChangeAction::Create,
                    target: None,
                    content: Some(remember.content.clone()),
                    scope: remember.context.write_scope.clone().unwrap(),
                    sources: vec![],
                    metadata: remember.metadata.clone(),
                }],
                effects: vec![Effect::ChangeMemory],
                expires_at: None,
            })
        }
        async fn apply(&self, request: &ApplyRequest, _: &dyn OperationControl) -> Result<Receipt> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.receipt(&request.request_id))
        }
        async fn close(&self, _: &dyn OperationControl) -> Result<()> {
            self.close_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_close {
                Err(err(ErrorKind::OutcomeUnknown))
            } else {
                Ok(())
            }
        }
    }
    fn context(origin: Origin) -> MemoryContext {
        let scope = Scope {
            namespace: "user".into(),
            id: "u1".into(),
        };
        MemoryContext {
            principal: "human".into(),
            scopes: vec![scope.clone()],
            write_scope: Some(scope),
            workspace_id: None,
            session_id: None,
            task_id: None,
            origin,
        }
    }
    fn remember(origin: Origin) -> RememberRequest {
        RememberRequest {
            context: context(origin),
            request_id: "r1".into(),
            content: Content::Text("use tabs".into()),
            target: None,
            metadata: BTreeMap::new(),
        }
    }
    fn grant() -> LearningGrant {
        LearningGrant {
            scopes: context(Origin::Human).scopes,
            input_classes: vec!["explicit_memory".into()],
            background: false,
        }
    }
    fn recall_request() -> RecallRequest {
        RecallRequest {
            context: context(Origin::Model),
            query: "tabs".into(),
            limit: 10,
            token_budget: 1000,
        }
    }

    #[test]
    fn injected_exact_guard_rechecks_description_without_remote_effects() {
        let captured_secret = "captured-secret-value".to_owned();
        let mut raw = Fake::new(&[Operation::Remember]);
        raw.description.identity = captured_secret.clone();
        let fake = Arc::new(raw);
        let client = Client::new(fake.clone()).unwrap();
        let guard = Arc::new(
            super::super::guard::ExactGuard::new(std::slice::from_ref(&captured_secret)).unwrap(),
        );
        let error = match client.with_guard(guard) {
            Ok(_) => panic!("captured secret must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::Rejected);
        assert_eq!(error.detail, Error::new(ErrorKind::Rejected).detail);
        assert!(!error.to_string().contains(&captured_secret));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.close_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_unsupported_model_background_and_canceled_calls_before_service() {
        let token = CancellationToken::new();
        let fake = Arc::new(Fake::new(&[]));
        let client = Client::new(fake.clone()).unwrap();
        assert_eq!(
            client
                .remember(&remember(Origin::Human), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        let mut raw = Fake::new(&[Operation::Remember]);
        raw.description.capabilities[1]
            .effects
            .push(Effect::BackgroundEvolution);
        let fake = Arc::new(raw);
        let client = Client::new(fake.clone()).unwrap();
        let mut request = remember(Origin::Model);
        request.metadata.insert("approved".into(), "true".into());
        assert_eq!(
            client.remember(&request, &token).await.unwrap_err().kind,
            ErrorKind::Unauthorized
        );
        assert_eq!(
            client
                .remember(&remember(Origin::Human), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unauthorized
        );
        token.cancel();
        assert_eq!(
            client
                .recall(&recall_request(), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Canceled
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn learning_requires_declared_input_scopes_and_background_permission() {
        let token = CancellationToken::new();
        let fake = Arc::new(Fake::new(&[Operation::Remember, Operation::Observe]));
        let client = Client::with_learning(fake.clone(), grant()).unwrap();
        client
            .remember(&remember(Origin::Model), &token)
            .await
            .unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let mut wrong = remember(Origin::Human);
        wrong.context.scopes[0].id = "other".into();
        wrong.context.write_scope = Some(wrong.context.scopes[0].clone());
        assert_eq!(
            client.remember(&wrong, &token).await.unwrap_err().kind,
            ErrorKind::Unauthorized
        );
        let observe = ObserveRequest {
            context: context(Origin::Model),
            request_id: "o1".into(),
            mode: ObserveMode::Learn,
            observations: vec![Observation {
                event_id: "event-1".into(),
                content: Content::Text("hello".into()),
                source: SourceRef {
                    id: "source-1".into(),
                    role: "user".into(),
                    occurred_at: None,
                    trust: Trust::Local,
                },
            }],
        };
        assert_eq!(
            client.observe(&observe, &token).await.unwrap_err().kind,
            ErrorKind::Unauthorized
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let mut raw = Fake::new(&[Operation::Remember]);
        raw.description.capabilities[1]
            .effects
            .push(Effect::BackgroundEvolution);
        let fake = Arc::new(raw);
        let client = Client::with_learning(fake.clone(), grant()).unwrap();
        assert_eq!(
            client
                .remember(&remember(Origin::Model), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unauthorized
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn validates_returned_scopes_and_rejects_secrets() {
        let token = CancellationToken::new();
        let mut raw = Fake::new(&[Operation::Remember]);
        raw.recall = Some(RecallResult {
            items: vec![RecallItem {
                reference: Some(MemoryRef {
                    service: "fake-service".into(),
                    scope: Scope {
                        namespace: "user".into(),
                        id: "other".into(),
                    },
                    id: "item-1".into(),
                    revision: None,
                }),
                content: Content::Text("plain".into()),
                sources: vec![],
                effective_at: None,
                metadata: BTreeMap::new(),
            }],
            used_tokens: None,
            coverage: Coverage::Complete,
        });
        let fake = Arc::new(raw);
        let client = Client::new(fake.clone()).unwrap();
        assert_eq!(
            client
                .recall(&recall_request(), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unauthorized
        );
        let mut secret = remember(Origin::Human);
        secret
            .metadata
            .insert("note".into(), "password=hunter2".into());
        assert_eq!(
            client.remember(&secret, &token).await.unwrap_err().kind,
            ErrorKind::Rejected
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn application_binds_original_snapshot_and_consumes_completed_reference() {
        let fake = Arc::new(Fake::new(&[Operation::Prepare, Operation::Apply]));
        let client = Client::new(fake.clone()).unwrap();
        let token = CancellationToken::new();
        let snapshot = client
            .prepare(
                &PrepareRequest {
                    operation: MutatingRequest::Remember(remember(Origin::Model)),
                },
                &token,
            )
            .await
            .unwrap();
        let apply = ApplyRequest {
            context: context(Origin::Human),
            request_id: "a1".into(),
            preparation: snapshot.reference.clone(),
        };
        let mut changed = snapshot.clone();
        changed.changes[0].content = Some(Content::Text("changed after review".into()));
        assert_eq!(
            client
                .apply(&changed, &apply, &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        client.apply(&snapshot, &apply, &token).await.unwrap();
        assert_eq!(
            client
                .apply(&snapshot, &apply, &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
        client
            .restore_prepared(
                snapshot.clone(),
                &context(Origin::Human),
                "incorrect-digest",
                PreparationRecovery::default(),
            )
            .unwrap_err();
        client
            .restore_prepared(
                snapshot.clone(),
                &context(Origin::Human),
                &prepared_digest(&snapshot).unwrap(),
                PreparationRecovery::default(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn unknown_partial_results_and_failed_close_keep_their_actual_state() {
        for completion in [Completion::Partial, Completion::Unknown] {
            let mut raw = Fake::new(&[Operation::Remember]);
            raw.completion = completion;
            raw.fail_close = true;
            let fake = Arc::new(raw);
            let client = Client::new(fake.clone())
                .unwrap()
                .with_provider_shutdown(true);
            let token = CancellationToken::new();
            let receipt = client
                .remember(&remember(Origin::Human), &token)
                .await
                .unwrap();
            assert_eq!(receipt.completion, completion);
            assert_eq!(receipt.visibility, Visibility::Unknown);
            assert_eq!(
                client.close(&token).await.unwrap_err().kind,
                ErrorKind::OutcomeUnknown
            );
            assert_eq!(
                client.close(&token).await.unwrap_err().kind,
                ErrorKind::OutcomeUnknown
            );
            assert_eq!(fake.close_calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                client
                    .recall(&recall_request(), &token)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Closed
            );
        }
    }

    #[tokio::test]
    async fn invalid_completion_and_secret_output_are_not_accepted() {
        let token = CancellationToken::new();
        let mut raw = Fake::new(&[Operation::Remember]);
        raw.bad_receipt = true;
        let fake = Arc::new(raw);
        let client = Client::new(fake.clone()).unwrap();
        assert_eq!(
            client
                .remember(&remember(Origin::Human), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidInput
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        let mut raw = Fake::new(&[]);
        raw.recall = Some(RecallResult {
            items: vec![RecallItem {
                reference: None,
                content: Content::Text("password=hunter2".into()),
                sources: vec![],
                effective_at: None,
                metadata: BTreeMap::new(),
            }],
            used_tokens: None,
            coverage: Coverage::Unknown,
        });
        let client = Client::new(Arc::new(raw)).unwrap();
        assert_eq!(
            client
                .recall(&recall_request(), &token)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Rejected
        );
    }

    #[tokio::test]
    async fn workspace_learning_grant_preserves_read_authority_and_narrows_remote_write() {
        let workspace = Scope {
            namespace: "workspace".into(),
            id: "project-1".into(),
        };
        let fake = Arc::new(Fake::new(&[Operation::Remember]));
        let client = Client::with_learning(
            fake.clone(),
            LearningGrant {
                scopes: vec![workspace.clone()],
                input_classes: vec!["explicit_memory".into()],
                background: false,
            },
        )
        .unwrap();
        let mut request = remember(Origin::Model);
        request.context.scopes.push(workspace.clone());
        request.context.write_scope = Some(workspace.clone());
        client
            .remember(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.remember_contexts.lock().unwrap()[0].scopes,
            vec![workspace]
        );
        assert_eq!(request.context.scopes.len(), 2);
    }

    #[tokio::test]
    async fn workspace_write_cannot_return_user_scope_effect() {
        let workspace = Scope {
            namespace: "workspace".into(),
            id: "project-1".into(),
        };
        let mut raw = Fake::new(&[Operation::Remember]);
        raw.remember_reference = Some(MemoryRef {
            service: "fake-service".into(),
            scope: context(Origin::Human).scopes[0].clone(),
            id: "user-effect".into(),
            revision: Some("1".into()),
        });
        let fake = Arc::new(raw);
        let client = Client::with_learning(
            fake.clone(),
            LearningGrant {
                scopes: vec![workspace.clone()],
                input_classes: vec!["explicit_memory".into()],
                background: false,
            },
        )
        .unwrap();
        let mut request = remember(Origin::Model);
        request.context.scopes.push(workspace.clone());
        request.context.write_scope = Some(workspace.clone());
        assert_eq!(
            client
                .remember(&request, &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unauthorized
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.remember_contexts.lock().unwrap()[0].scopes,
            vec![workspace]
        );
    }
}
