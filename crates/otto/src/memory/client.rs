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
use crate::hex::to_hex;

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
            organization_id: None,
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
                "message" | "tool_result" | "task" | "explicit_memory" | "feedback"
            )
        }) {
            return Err(err(ErrorKind::InvalidInput));
        }
        client.mode = AuthorizationMode::DelegatedLearning;
        client.grant = Some(grant);
        Ok(client)
    }

    pub(crate) fn with_guard(mut self, guard: Arc<dyn ContentGuard>) -> Result<Self> {
        self.guard = guard;
        self.guarded(&self.description)?;
        Ok(self)
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
    pub(crate) fn is_closed(&self) -> bool {
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

    /// Whether the content guard admits `value`.
    pub(crate) fn guard_admits(&self, value: &impl serde::Serialize) -> bool {
        self.guarded(value).is_ok()
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
        effective.context = self.effective_context(&request.context, vec![scope.clone()])?;
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

    /// Explicit human management entry point. Never exposed as a model capability.
    /// The host selects the tenant; the service remains responsible for administrator ACLs.
    pub(crate) async fn remember_organization_human(
        &self,
        request: &RememberRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate(&self.description.identity).map_err(safe)?;
        self.admit(Operation::Remember, control)?;
        let scope = request
            .target
            .as_ref()
            .map(|target| &target.scope)
            .or(request.context.write_scope.as_ref())
            .ok_or_else(|| err(ErrorKind::InvalidInput))?;
        if request.context.origin != Origin::Human
            || request.context.organization_id.is_none()
            || scope.namespace != "organization"
            || request.context.organization_id.as_deref() != Some(scope.id.as_str())
        {
            return Err(err(ErrorKind::Unauthorized));
        }
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
        effective.context = self.effective_context(&request.context, scopes.clone())?;
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
    Ok(to_hex(&Sha256::digest(bytes)))
}

fn err(kind: ErrorKind) -> Error {
    Error::new(kind)
}
fn safe(error: Error) -> Error {
    Error::new(error.kind)
}
fn input_class(source: &SourceRef) -> Result<&'static str> {
    match source.role.as_str() {
        "user" | "assistant" | "system" | "message" | "tool_call" | "context" => Ok("message"),
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

mod prepared;

#[cfg(test)]
mod tests;
