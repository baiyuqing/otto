//! Built-in projection of the service-neutral memory port.
//!
//! `new` borrows the shared domain service: closing this adapter only releases
//! its binding. `owned` transfers shutdown responsibility; its close closes
//! the service for every outstanding reference. Neither operation deletes data.
//! Synchronous repository calls run on Tokio's blocking pool. Once admitted,
//! a write is awaited to its actual result rather than abandoned and replayed.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use chrono::Utc;
use otto_core::{memory as port, operation::OperationControl};
use sha2::{Digest, Sha256};

use super::{Candidate, CandidateAction, CandidateState, Record, Scope, Service};

pub struct BuiltinAdapter {
    service: Arc<Service>,
    identity: String,
    encryption_at_rest: bool,
    owned: bool,
    closed: AtomicBool,
}

impl BuiltinAdapter {
    pub fn new(service: Arc<Service>) -> port::Result<Self> {
        Self::construct(service, false)
    }
    pub fn owned(service: Arc<Service>) -> port::Result<Self> {
        Self::construct(service, true)
    }
    fn construct(service: Arc<Service>, owned: bool) -> port::Result<Self> {
        let identity = service.identity().map_err(domain_error)?.database_id;
        let encryption_at_rest = service.encryption_at_rest().map_err(domain_error)?;
        if !port::valid_opaque_id(&identity, port::MAX_ID_BYTES) {
            return Err(error(port::ErrorKind::InvalidInput));
        }
        Ok(Self {
            service,
            identity,
            encryption_at_rest,
            owned,
            closed: AtomicBool::new(false),
        })
    }

    fn admit(&self, control: &dyn OperationControl) -> port::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(error(port::ErrorKind::Closed));
        }
        if control.admission_stop_reason().is_some()
            || control.cancellation_token().is_cancelled()
            || control
                .remaining()
                .is_some_and(|remaining| remaining.is_zero())
        {
            return Err(error(port::ErrorKind::Canceled));
        }
        Ok(())
    }

    async fn run<T: Send + 'static>(
        &self,
        control: &dyn OperationControl,
        operation: impl FnOnce(Arc<Service>) -> port::Result<T> + Send + 'static,
    ) -> port::Result<T> {
        self.admit(control)?;
        let service = self.service.clone();
        let cancel = control.cancellation_token().clone();
        let deadline = control
            .remaining()
            .and_then(|remaining| std::time::Instant::now().checked_add(remaining));
        tokio::task::spawn_blocking(move || {
            if cancel.is_cancelled()
                || deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline)
            {
                return Err(error(port::ErrorKind::Canceled));
            }
            operation(service)
        })
        .await
        .map_err(|_| error(port::ErrorKind::OutcomeUnknown))?
    }

    fn reference(&self, scope: &Scope, id: String, revision: u64) -> port::MemoryRef {
        reference(&self.identity, scope, id, revision)
    }
}

fn error(kind: port::ErrorKind) -> port::Error {
    port::Error::new(kind)
}
fn domain_error(native_error: super::Error) -> port::Error {
    use super::ErrorKind as Native;
    error(match native_error.kind {
        Native::Conflict | Native::NotFound | Native::InvalidCursor => port::ErrorKind::Conflict,
        Native::Unsupported => port::ErrorKind::Unsupported,
        Native::InvalidRequest | Native::InvalidRecord => port::ErrorKind::InvalidInput,
        Native::SensitiveMemory | Native::PolicyDecision => port::ErrorKind::Rejected,
        Native::CommitUnknown => port::ErrorKind::OutcomeUnknown,
        Native::Canceled => port::ErrorKind::Canceled,
        Native::Closed => port::ErrorKind::Closed,
        _ => port::ErrorKind::Unavailable,
    })
}
fn native_scope(scope: &port::Scope) -> Scope {
    Scope::new(&scope.namespace, &scope.id)
}
fn core_scope(scope: &Scope) -> port::Scope {
    port::Scope {
        namespace: scope.namespace.clone(),
        id: scope.id.clone(),
    }
}
fn reference(service: &str, scope: &Scope, id: String, revision: u64) -> port::MemoryRef {
    port::MemoryRef {
        organization_id: None,
        service: service.into(),
        scope: core_scope(scope),
        id,
        revision: Some(revision.to_string()),
    }
}
fn revision(reference: &port::MemoryRef) -> port::Result<u64> {
    reference
        .revision
        .as_ref()
        .and_then(|r| r.parse::<u64>().ok())
        .filter(|r| *r > 0)
        .ok_or_else(|| error(port::ErrorKind::Conflict))
}
fn provenance(context: &port::MemoryContext) -> super::Provenance {
    super::Provenance {
        origin: Some(match context.origin {
            port::Origin::Human => super::Origin::Human,
            port::Origin::Model => super::Origin::Model,
            port::Origin::Extractor => super::Origin::Extractor,
        }),
        session_id: context.session_id.clone().unwrap_or_default(),
        ..Default::default()
    }
}
fn sources(source: &super::Provenance) -> Vec<port::SourceRef> {
    let role = source
        .origin
        .map_or("unknown", |origin| origin.as_str())
        .to_owned();
    source
        .message_ids
        .iter()
        .chain((!source.observation_id.is_empty()).then_some(&source.observation_id))
        .map(|id| port::SourceRef {
            id: id.clone(),
            role: role.clone(),
            occurred_at: None,
            trust: port::Trust::External,
        })
        .collect()
}
fn item(service: &str, record: Record, standing: bool) -> port::RecallItem {
    let mut metadata = record.metadata;
    metadata.insert("otto.kind".into(), record.kind);
    metadata.insert("otto.key".into(), record.key);
    metadata.insert("otto.standing".into(), standing.to_string());
    port::RecallItem {
        remote: None,
        reference: Some(reference(
            service,
            &record.scope,
            record.id,
            record.revision,
        )),
        content: port::Content::Text(record.text),
        sources: sources(&record.source),
        effective_at: Some(record.updated_at.timestamp()),
        metadata,
    }
}
fn remember(request: &port::RememberRequest) -> port::Result<super::RememberRequest> {
    let port::Content::Text(text) = &request.content else {
        return Err(error(port::ErrorKind::Unsupported));
    };
    let scope = request
        .target
        .as_ref()
        .map(|r| &r.scope)
        .or(request.context.write_scope.as_ref())
        .ok_or_else(|| error(port::ErrorKind::InvalidInput))?;
    request.context.validate_scope(scope)?;
    let mut metadata = request.metadata.clone();
    let kind = metadata
        .remove("otto.kind")
        .unwrap_or_else(|| "fact".into());
    let key = request
        .key
        .clone()
        .unwrap_or_else(|| metadata.remove("otto.key").unwrap_or_default());
    Ok(super::RememberRequest {
        scope: native_scope(scope),
        kind,
        key,
        text: text.clone(),
        metadata,
        id: request
            .target
            .as_ref()
            .map(|r| r.id.clone())
            .unwrap_or_default(),
        expected_revision: request.target.as_ref().map(revision).transpose()?,
        source: provenance(&request.context),
        ..Default::default()
    })
}
fn forget(request: &port::ForgetRequest) -> port::Result<super::ForgetRequest> {
    if request.coverage != port::ForgetLevel::DeleteActive {
        return Err(error(port::ErrorKind::Unsupported));
    }
    let port::ForgetTarget::Item(target) = &request.target else {
        return Err(error(port::ErrorKind::Unsupported));
    };
    Ok(super::ForgetRequest {
        reference: super::RecordRef {
            scope: native_scope(&target.scope),
            id: target.id.clone(),
        },
        expected_revision: revision(target)?,
        purge_backups: false,
        confirm_purge: false,
    })
}
fn human(context: &port::MemoryContext) -> port::Result<()> {
    if context.origin == port::Origin::Human {
        Ok(())
    } else {
        Err(error(port::ErrorKind::Unauthorized))
    }
}
fn receipt(request_id: &str, reference: port::MemoryRef, forgotten: bool) -> port::Receipt {
    port::Receipt {
        request_id: request_id.into(),
        operation_ref: None,
        admission: port::Admission::Acknowledged,
        completion: port::Completion::Complete,
        visibility: if forgotten {
            port::Visibility::NotApplicable
        } else {
            port::Visibility::Ready
        },
        items: vec![port::ItemOutcome {
            reference: Some(reference),
            state: port::ItemState::Applied,
            reason: None,
        }],
        forget_coverage: forgotten.then(|| port::ForgetCoverage {
            achieved: vec![port::ForgetLevel::DeleteActive],
            remaining: vec![port::ForgetLevel::DeleteDerived],
        }),
    }
}
fn fingerprint(service: &str, candidate: &Candidate) -> port::Result<String> {
    let proposed =
        super::proposed::encode_proposed_record(&candidate.proposed).map_err(domain_error)?;
    let bytes = serde_json::to_vec(&(
        service,
        proposed,
        candidate.action.as_str(),
        &candidate.target_id,
        candidate.base_revision,
        &candidate.reason,
        candidate.created_at.timestamp_micros(),
    ))
    .map_err(|_| error(port::ErrorKind::InvalidInput))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[async_trait::async_trait]
impl port::MemoryService for BuiltinAdapter {
    fn describe(&self) -> port::ServiceDescription {
        let capabilities = [
            port::Operation::Recall,
            port::Operation::Inspect,
            port::Operation::Remember,
            port::Operation::Forget,
            port::Operation::Prepare,
            port::Operation::Apply,
        ]
        .into_iter()
        .map(|operation| {
            let mut capability = port::Capability::new(operation);
            capability.supports_reviewed_changes = true;
            if operation == port::Operation::Prepare {
                capability.effects.push(port::Effect::StoreEvidence);
            }
            if operation == port::Operation::Forget {
                capability.forget_targets.push(port::ForgetTargetKind::Item);
                capability
                    .forget_levels
                    .push(port::ForgetLevel::DeleteActive);
            }
            if matches!(
                operation,
                port::Operation::Remember | port::Operation::Forget | port::Operation::Apply
            ) {
                capability.effects.push(port::Effect::ChangeMemory);
                capability.conditional_updates = true;
            }
            capability
        })
        .collect();
        port::ServiceDescription {
            identity: self.identity.clone(),
            encryption_at_rest: Some(self.encryption_at_rest),
            version: 1,
            capabilities,
        }
    }

    async fn recall(
        &self,
        request: &port::RecallRequest,
        control: &dyn OperationControl,
    ) -> port::Result<port::RecallResult> {
        request.validate()?;
        let request_copy = request.clone();
        let identity = self.identity.clone();
        let result = self
            .run(control, move |service| {
                let binding = service
                    .bind(super::BindOptions {
                        scopes: request_copy
                            .context
                            .scopes
                            .iter()
                            .map(native_scope)
                            .collect(),
                        default_write_scope: native_scope(
                            request_copy
                                .context
                                .write_scope
                                .as_ref()
                                .unwrap_or(&request_copy.context.scopes[0]),
                        ),
                        ..Default::default()
                    })
                    .map_err(domain_error)?;
                let recalled = binding
                    .recall(&super::RecallRequest {
                        query: request_copy.query,
                        kinds: vec![],
                        limit: request_copy.limit,
                        token_budget: request_copy.token_budget,
                    })
                    .map_err(domain_error)?;
                binding.release();
                let mut items = Vec::new();
                let mut used = 0;
                for record in recalled.records {
                    let standing = recalled.standing.contains(&record.id);
                    let projected = item(&identity, record, standing);
                    // Include reference, provenance and metadata bytes in admission,
                    // using the built-in conservative three-bytes-per-token estimate.
                    let cost = serde_json::to_vec(&projected)
                        .map_err(|_| error(port::ErrorKind::InvalidInput))?
                        .len()
                        .div_ceil(3)
                        .max(1);
                    if items.len() == request_copy.limit || used + cost > request_copy.token_budget
                    {
                        continue;
                    }
                    used += cost;
                    items.push(projected);
                }
                Ok(port::RecallResult {
                    items,
                    used_tokens: Some(used),
                    coverage: port::Coverage::Partial,
                })
            })
            .await?;
        result.validate(request, &self.identity)?;
        Ok(result)
    }

    async fn inspect(
        &self,
        request: &port::InspectRequest,
        control: &dyn OperationControl,
    ) -> port::Result<port::InspectResult> {
        request.validate(&self.identity)?;
        let query = request.query.clone();
        let identity = self.identity.clone();
        let result = self
            .run(control, move |service| {
                let mut result = port::InspectResult {
                    items: vec![],
                    next_cursor: String::new(),
                    state: "ready".into(),
                };
                match query {
                    port::InspectQuery::Get(reference) => {
                        let record = service
                            .get(&super::RecordRef {
                                scope: native_scope(&reference.scope),
                                id: reference.id.clone(),
                            })
                            .map_err(domain_error)?;
                        if reference
                            .revision
                            .as_ref()
                            .is_some_and(|r| *r != record.revision.to_string())
                        {
                            return Err(error(port::ErrorKind::Conflict));
                        }
                        result.items.push(item(&identity, record, false));
                    }
                    port::InspectQuery::List {
                        scope,
                        limit,
                        cursor,
                    } => {
                        let page = service
                            .list(&super::ListRequest {
                                all_scopes: false,
                                scopes: vec![native_scope(&scope)],
                                kinds: vec![],
                                labels: vec![],
                                limit,
                                cursor,
                                now: Utc::now(),
                                include_expired: false,
                            })
                            .map_err(domain_error)?;
                        result.items = page
                            .records
                            .into_iter()
                            .map(|record| item(&identity, record, false))
                            .collect();
                        result.next_cursor = page.next_cursor;
                    }
                    port::InspectQuery::Status => {
                        service.identity().map_err(domain_error)?;
                    }
                }
                Ok(result)
            })
            .await?;
        result.validate(request, &self.identity)?;
        Ok(result)
    }

    async fn remember(
        &self,
        request: &port::RememberRequest,
        control: &dyn OperationControl,
    ) -> port::Result<port::Receipt> {
        request.validate(&self.identity)?;
        human(&request.context)?;
        let native = remember(request)?;
        let record = self
            .run(control, move |service| {
                service.remember(&native).map_err(domain_error)
            })
            .await?;
        let result = receipt(
            &request.request_id,
            self.reference(&record.scope, record.id, record.revision),
            false,
        );
        result.validate(&request.context, &request.request_id, &self.identity)?;
        Ok(result)
    }

    async fn forget(
        &self,
        request: &port::ForgetRequest,
        control: &dyn OperationControl,
    ) -> port::Result<port::Receipt> {
        request.validate(&self.identity)?;
        human(&request.context)?;
        let native = forget(request)?;
        let forgotten = self
            .run(control, move |service| {
                service.forget(&native).map_err(domain_error)
            })
            .await?
            .tombstone;
        let result = receipt(
            &request.request_id,
            self.reference(&forgotten.scope, forgotten.id, forgotten.revision),
            true,
        );
        result.validate(&request.context, &request.request_id, &self.identity)?;
        Ok(result)
    }

    async fn prepare(
        &self,
        request: &port::PrepareRequest,
        control: &dyn OperationControl,
    ) -> port::Result<port::PreparedChanges> {
        request.validate(&self.identity)?;
        let context = request.operation.context().clone();
        if context.origin == port::Origin::Human {
            return Err(error(port::ErrorKind::Unauthorized));
        }
        let proposal = match &request.operation {
            port::MutatingRequest::Remember(request) => {
                let native = remember(request)?;
                super::ProposeRequest {
                    action: if native.expected_revision.is_some() {
                        CandidateAction::Update
                    } else {
                        CandidateAction::Create
                    },
                    target_id: native.id,
                    base_revision: native.expected_revision.unwrap_or(0),
                    scope: native.scope,
                    kind: native.kind,
                    key: native.key,
                    text: native.text,
                    metadata: native.metadata,
                    source: native.source,
                    reason: format!("otto.port.principal:{}", context.principal),
                    ..Default::default()
                }
            }
            port::MutatingRequest::Forget(request) => {
                let native = forget(request)?;
                super::ProposeRequest {
                    action: CandidateAction::Forget,
                    scope: native.reference.scope,
                    target_id: native.reference.id,
                    base_revision: native.expected_revision,
                    source: provenance(&context),
                    reason: format!("otto.port.principal:{}", context.principal),
                    ..Default::default()
                }
            }
            _ => return Err(error(port::ErrorKind::Unsupported)),
        };
        let candidate = self
            .run(control, move |service| {
                service
                    .propose(&proposal)
                    .map_err(domain_error)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| error(port::ErrorKind::Rejected))
            })
            .await?;
        let change = port::Change {
            action: match candidate.action {
                CandidateAction::Create => port::ChangeAction::Create,
                CandidateAction::Update => port::ChangeAction::Update,
                CandidateAction::Forget => port::ChangeAction::Forget,
            },
            target: (candidate.action != CandidateAction::Create).then(|| {
                self.reference(
                    &candidate.proposed.scope,
                    candidate.target_id.clone(),
                    candidate.base_revision,
                )
            }),
            content: (candidate.action != CandidateAction::Forget)
                .then(|| port::Content::Text(candidate.proposed.text.clone())),
            scope: core_scope(&candidate.proposed.scope),
            sources: sources(&candidate.proposed.source),
            metadata: if candidate.action == CandidateAction::Forget {
                Default::default()
            } else {
                item(&self.identity, candidate.proposed.clone(), false).metadata
            },
        };
        let result = port::PreparedChanges {
            reference: format!(
                "{}:{}:{}",
                self.identity,
                candidate.id,
                fingerprint(&self.identity, &candidate)?
            ),
            context,
            changes: vec![change],
            effects: vec![port::Effect::ChangeMemory],
            expires_at: None,
        };
        result.validate(request, &self.identity)?;
        Ok(result)
    }

    async fn apply(
        &self,
        request: &port::ApplyRequest,
        control: &dyn OperationControl,
    ) -> port::Result<port::Receipt> {
        request.validate()?;
        human(&request.context)?;
        let parts: Vec<_> = request.preparation.split(':').collect();
        if parts.len() != 3 || parts[0] != self.identity {
            return Err(error(port::ErrorKind::Conflict));
        }
        let id = parts[1].to_owned();
        let digest = parts[2].to_owned();
        let scopes = request.context.scopes.clone();
        let principal = request.context.principal.clone();
        let identity = self.identity.clone();
        let reviewed = self
            .run(control, move |service| {
                let mut candidate = None;
                for scope in scopes {
                    match service.get_candidate(&super::CandidateRef {
                        scope: native_scope(&scope),
                        id: id.clone(),
                    }) {
                        Ok(found) => {
                            candidate = Some(found);
                            break;
                        }
                        Err(e) if e.kind == super::ErrorKind::NotFound => {}
                        Err(e) => return Err(domain_error(e)),
                    }
                }
                let candidate = candidate.ok_or_else(|| error(port::ErrorKind::Conflict))?;
                if candidate.state != CandidateState::Pending
                    || candidate.reason != format!("otto.port.principal:{principal}")
                    || fingerprint(&identity, &candidate)? != digest
                {
                    return Err(error(port::ErrorKind::Conflict));
                }
                service
                    .review(&super::ReviewRequest {
                        reference: super::CandidateRef {
                            scope: candidate.proposed.scope,
                            id: candidate.id,
                        },
                        decision: super::ReviewDecision::Accept,
                        edited: None,
                        target_revision: (candidate.action != CandidateAction::Create)
                            .then_some(candidate.base_revision),
                    })
                    .map_err(domain_error)
            })
            .await?;
        let (reference, forgotten) = if let Some(record) = reviewed.record {
            (
                self.reference(&record.scope, record.id, record.revision),
                false,
            )
        } else if let Some(tombstone) = reviewed.tombstone {
            (
                self.reference(&tombstone.scope, tombstone.id, tombstone.revision),
                true,
            )
        } else {
            return Err(error(port::ErrorKind::OutcomeUnknown));
        };
        let result = receipt(&request.request_id, reference, forgotten);
        result.validate(&request.context, &request.request_id, &self.identity)?;
        Ok(result)
    }

    async fn close(&self, control: &dyn OperationControl) -> port::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.admit(control)?;
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if self.owned {
            let service = self.service.clone();
            tokio::task::spawn_blocking(move || service.close().map_err(domain_error))
                .await
                .map_err(|_| error(port::ErrorKind::OutcomeUnknown))??;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use port::MemoryService;
    use tokio_util::sync::CancellationToken;

    fn setup() -> (
        tempfile::TempDir,
        Arc<Service>,
        BuiltinAdapter,
        port::MemoryContext,
    ) {
        let (directory, store) = super::super::files::testsupport::open_temp();
        let service = Arc::new(Service::new(store, super::super::decide_default_policy));
        let scope = core_scope(&service.identity().unwrap().user_scope);
        let context = port::MemoryContext {
            organization_id: None,
            principal: "human".into(),
            scopes: vec![scope.clone()],
            write_scope: Some(scope),
            workspace_id: None,
            session_id: None,
            task_id: None,
            origin: port::Origin::Human,
        };
        let adapter = BuiltinAdapter::new(service.clone()).unwrap();
        (directory, service, adapter, context)
    }
    fn request(context: port::MemoryContext) -> port::RememberRequest {
        port::RememberRequest {
            key: None,
            context,
            request_id: "remember-1".into(),
            content: port::Content::Text("apricot orchard".into()),
            target: None,
            metadata: Default::default(),
        }
    }
    async fn list(
        adapter: &BuiltinAdapter,
        context: &port::MemoryContext,
    ) -> Vec<port::RecallItem> {
        adapter
            .inspect(
                &port::InspectRequest {
                    context: context.clone(),
                    query: port::InspectQuery::List {
                        scope: context.scopes[0].clone(),
                        limit: 10,
                        cursor: String::new(),
                    },
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .items
    }

    #[tokio::test]
    async fn recall_inspect_and_shared_close_preserve_domain_records() {
        let (_directory, service, adapter, context) = setup();
        let cancel = CancellationToken::new();
        let written = adapter
            .remember(&request(context.clone()), &cancel)
            .await
            .unwrap();
        let reference = written.items[0].reference.clone().unwrap();
        assert_eq!(reference.service, service.identity().unwrap().database_id);
        assert_eq!(list(&adapter, &context).await.len(), 1);
        let recalled = adapter
            .recall(
                &port::RecallRequest {
                    scope_filter: None,
                    context: context.clone(),
                    query: "apricot".into(),
                    limit: 10,
                    token_budget: 1000,
                },
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(recalled.items.len(), 1);
        assert_eq!(recalled.items[0].metadata["otto.kind"], "fact");
        let mut wrong = context.clone();
        wrong.scopes = vec![port::Scope {
            namespace: "workspace".into(),
            id: "other".into(),
        }];
        wrong.write_scope = Some(wrong.scopes[0].clone());
        assert!(list(&adapter, &wrong).await.is_empty());
        adapter.close(&cancel).await.unwrap();
        assert!(
            service
                .get(&super::super::RecordRef {
                    scope: native_scope(&reference.scope),
                    id: reference.id
                })
                .is_ok()
        );
    }

    #[tokio::test]
    async fn model_writes_require_persisted_review_and_application() {
        let (_directory, _service, adapter, mut context) = setup();
        let cancel = CancellationToken::new();
        context.origin = port::Origin::Model;
        assert_eq!(
            adapter
                .remember(&request(context.clone()), &cancel)
                .await
                .unwrap_err()
                .kind,
            port::ErrorKind::Unauthorized
        );
        let prepared = adapter
            .prepare(
                &port::PrepareRequest {
                    operation: port::MutatingRequest::Remember(request(context.clone())),
                },
                &cancel,
            )
            .await
            .unwrap();
        assert!(list(&adapter, &context).await.is_empty());
        context.origin = port::Origin::Human;
        let mut apply = port::ApplyRequest {
            context: context.clone(),
            request_id: "apply-1".into(),
            preparation: prepared.reference,
        };
        let saved = apply.preparation.clone();
        apply.preparation = format!("other:{}", saved.split_once(':').unwrap().1);
        assert_eq!(
            adapter.apply(&apply, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Conflict
        );
        apply.preparation = saved;
        apply.context.principal = "other-human".into();
        assert_eq!(
            adapter.apply(&apply, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Conflict
        );
        apply.context = context.clone();
        let receipt = adapter.apply(&apply, &cancel).await.unwrap();
        assert_eq!(list(&adapter, &context).await.len(), 1);
        assert_eq!(
            adapter.apply(&apply, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Conflict
        );
        let target = receipt.items[0].reference.clone().unwrap();
        context.origin = port::Origin::Model;
        let forget = port::ForgetRequest {
            context,
            request_id: "forget-model".into(),
            target: port::ForgetTarget::Item(target),
            coverage: port::ForgetLevel::DeleteActive,
        };
        assert_eq!(
            adapter.forget(&forget, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Unauthorized
        );
    }

    #[tokio::test]
    async fn wrong_scope_stale_revision_and_unsupported_forget_have_no_effect() {
        let (_directory, _service, adapter, context) = setup();
        let cancel = CancellationToken::new();
        let written = adapter
            .remember(&request(context.clone()), &cancel)
            .await
            .unwrap();
        let mut target = written.items[0].reference.clone().unwrap();
        let original = target.clone();
        target.revision = Some("999".into());
        let mut forget = port::ForgetRequest {
            context: context.clone(),
            request_id: "forget-1".into(),
            target: port::ForgetTarget::Item(target),
            coverage: port::ForgetLevel::DeleteActive,
        };
        assert_eq!(
            adapter.forget(&forget, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Conflict
        );
        forget.target = port::ForgetTarget::Item(original.clone());
        forget.coverage = port::ForgetLevel::DeleteDerived;
        assert_eq!(
            adapter.forget(&forget, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Unsupported
        );
        forget.target = port::ForgetTarget::Scope(context.scopes[0].clone());
        forget.coverage = port::ForgetLevel::DeleteActive;
        assert_eq!(
            adapter.forget(&forget, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Unsupported
        );
        forget.target = port::ForgetTarget::Source(port::SourceRef {
            id: "source-1".into(),
            role: "human".into(),
            occurred_at: None,
            trust: port::Trust::External,
        });
        assert_eq!(
            adapter.forget(&forget, &cancel).await.unwrap_err().kind,
            port::ErrorKind::Unsupported
        );
        let mut model = context.clone();
        model.origin = port::Origin::Model;
        let prepared = adapter
            .prepare(
                &port::PrepareRequest {
                    operation: port::MutatingRequest::Remember(request(model)),
                },
                &cancel,
            )
            .await
            .unwrap();
        let mut wrong = context.clone();
        wrong.scopes[0].id = "different".into();
        wrong.write_scope = Some(wrong.scopes[0].clone());
        assert_eq!(
            adapter
                .apply(
                    &port::ApplyRequest {
                        context: wrong,
                        request_id: "wrong-scope".into(),
                        preparation: prepared.reference
                    },
                    &cancel
                )
                .await
                .unwrap_err()
                .kind,
            port::ErrorKind::Conflict
        );
        assert_eq!(list(&adapter, &context).await.len(), 1);
    }

    #[tokio::test]
    async fn cancellation_and_resource_content_are_rejected_before_effects() {
        let (_directory, _service, adapter, context) = setup();
        let canceled = CancellationToken::new();
        canceled.cancel();
        assert_eq!(
            adapter
                .remember(&request(context.clone()), &canceled)
                .await
                .unwrap_err()
                .kind,
            port::ErrorKind::Canceled
        );
        let mut resource = request(context.clone());
        resource.content = port::Content::Resource {
            uri: "file:///example".into(),
            media_type: "text/plain".into(),
        };
        assert_eq!(
            adapter
                .remember(&resource, &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            port::ErrorKind::Unsupported
        );
        assert!(list(&adapter, &context).await.is_empty());
    }
}
