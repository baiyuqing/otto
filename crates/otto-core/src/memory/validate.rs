use std::collections::HashSet;

use super::*;

pub const MAX_CONTENT_BYTES: usize = 8 * 1024;
pub const MAX_QUERY_BYTES: usize = 8 * 1024;
pub const MAX_ITEMS: usize = 64;
pub const MAX_SCOPES: usize = 16;
pub const MAX_METADATA_ENTRIES: usize = 32;
pub const MAX_METADATA_BYTES: usize = 4 * 1024;
/// Management projections may add bounded service-specific semantic fields.
pub const MAX_PROJECTION_METADATA_ENTRIES: usize = 64;
pub const MAX_PROJECTION_METADATA_BYTES: usize = 8 * 1024;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
pub const MAX_TOKEN_BUDGET: usize = 8192;
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_CURSOR_BYTES: usize = 4 * 1024;

fn invalid() -> Error {
    Error::new(ErrorKind::InvalidInput)
}
fn require(condition: bool) -> Result<()> {
    if condition { Ok(()) } else { Err(invalid()) }
}

/// Opaque identifiers are nonempty bounded printable ASCII with no spaces.
/// They are never interpreted as paths, addresses or provider credentials.
pub fn valid_opaque_id(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && value.bytes().all(|b| b.is_ascii_graphic())
}
/// Organization ids also allow ordinary spaces within the name.
pub fn valid_organization_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().any(|b| b != b' ')
        && value.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
}
fn id(value: &str) -> Result<()> {
    require(valid_opaque_id(value, MAX_ID_BYTES))
}
fn optional_id(value: &Option<String>) -> Result<()> {
    value.as_deref().map_or(Ok(()), id)
}
fn text(value: &str, max: usize, empty: bool) -> Result<()> {
    require(
        value.len() <= max
            && (empty || !value.trim().is_empty())
            && value
                .chars()
                .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')),
    )
}
fn count<T>(values: &[T]) -> Result<()> {
    require(values.len() <= MAX_ITEMS)
}
fn unique<T: Eq + std::hash::Hash>(values: &[T]) -> Result<()> {
    require(values.iter().collect::<HashSet<_>>().len() == values.len())
}
fn metadata(values: &std::collections::BTreeMap<String, String>) -> Result<()> {
    bounded_metadata(values, MAX_METADATA_ENTRIES, MAX_METADATA_BYTES)
}
fn projection_metadata(values: &std::collections::BTreeMap<String, String>) -> Result<()> {
    bounded_metadata(
        values,
        MAX_PROJECTION_METADATA_ENTRIES,
        MAX_PROJECTION_METADATA_BYTES,
    )
}
fn bounded_metadata(
    values: &std::collections::BTreeMap<String, String>,
    max_entries: usize,
    max_bytes: usize,
) -> Result<()> {
    require(values.len() <= max_entries)?;
    for (key, value) in values {
        require(valid_opaque_id(key, 64))?;
        text(value, 512, true)?;
    }
    let bytes = serde_json::to_vec(values).map_err(|_| invalid())?;
    require(bytes.len() <= max_bytes)
}
fn sources(values: &[SourceRef]) -> Result<()> {
    count(values)?;
    for source in values {
        source.validate()?;
    }
    Ok(())
}
fn observations(values: &[Observation]) -> Result<()> {
    count(values)?;
    let mut bytes = 0;
    let mut events = HashSet::new();
    for observation in values {
        observation.validate()?;
        require(events.insert(&observation.event_id))?;
        bytes += observation.content.byte_len();
    }
    require(bytes <= MAX_BATCH_BYTES)
}

impl Scope {
    pub fn validate(&self) -> Result<()> {
        require(valid_opaque_id(&self.namespace, 32))?;
        require(if self.namespace == "organization" {
            valid_organization_id(&self.id)
        } else {
            valid_opaque_id(&self.id, 128)
        })
    }
}
impl MemoryContext {
    pub fn validate(&self) -> Result<()> {
        id(&self.principal)?;
        if let Some(organization) = &self.organization_id {
            require(valid_organization_id(organization))?;
        }
        require(!self.scopes.is_empty() && self.scopes.len() <= MAX_SCOPES)?;
        unique(&self.scopes)?;
        for scope in &self.scopes {
            scope.validate()?;
        }
        if let Some(scope) = &self.write_scope {
            self.validate_scope(scope)?;
        }
        optional_id(&self.workspace_id)?;
        optional_id(&self.session_id)?;
        optional_id(&self.task_id)
    }
    /// Only compares with authority established by the host. It never admits
    /// a new scope based on a service-returned field.
    pub fn validate_scope(&self, scope: &Scope) -> Result<()> {
        scope.validate()?;
        if self.scopes.contains(scope) {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::Unauthorized))
        }
    }
}
impl MemoryRef {
    pub fn validate(&self, context: &MemoryContext, service: &str) -> Result<()> {
        id(&self.service)?;
        id(&self.id)?;
        optional_id(&self.revision)?;
        require(self.service == service)?;
        if self.organization_id != context.organization_id {
            return Err(Error::new(ErrorKind::Unauthorized));
        }
        context.validate_scope(&self.scope)
    }
}
impl SourceRef {
    pub fn validate(&self) -> Result<()> {
        id(&self.id)?;
        require(valid_opaque_id(&self.role, 64))
    }
}
impl Content {
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Text(value) => value.len(),
            Self::Resource { uri, media_type } => uri.len() + media_type.len(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        require(self.byte_len() <= MAX_CONTENT_BYTES)?;
        match self {
            Self::Text(value) => text(value, MAX_CONTENT_BYTES, false),
            Self::Resource { uri, media_type } => {
                require(valid_opaque_id(uri, MAX_CONTENT_BYTES))?;
                require(valid_opaque_id(media_type, 128))
            }
        }
    }
}
impl RecallItem {
    pub fn validate(&self, context: &MemoryContext, service: &str) -> Result<()> {
        if let Some(reference) = &self.reference {
            reference.validate(context, service)?;
        }
        self.content.validate()?;
        sources(&self.sources)?;
        projection_metadata(&self.metadata)
    }
}
fn items(values: &[RecallItem], context: &MemoryContext, service: &str) -> Result<()> {
    count(values)?;
    let mut bytes = 0;
    for item in values {
        item.validate(context, service)?;
        bytes += item.content.byte_len();
    }
    require(bytes <= MAX_BATCH_BYTES)
}
impl RecallRequest {
    pub fn validate(&self) -> Result<()> {
        self.context.validate()?;
        if let Some(filter) = &self.scope_filter {
            require(!filter.is_empty())?;
            unique(filter)?;
            for namespace in filter {
                require(
                    self.context
                        .scopes
                        .iter()
                        .any(|scope| scope.namespace == namespace.as_str()),
                )?;
            }
        }
        text(&self.query, MAX_QUERY_BYTES, true)?;
        require(self.query.is_empty() || !self.query.trim().is_empty())?;
        require(self.limit > 0 && self.limit <= MAX_ITEMS)?;
        require(self.token_budget > 0 && self.token_budget <= MAX_TOKEN_BUDGET)
    }
}
impl RecallResult {
    pub fn validate(&self, request: &RecallRequest, service: &str) -> Result<()> {
        request.validate()?;
        require(self.items.len() <= request.limit)?;
        require(
            self.used_tokens
                .is_none_or(|used| used <= request.token_budget),
        )?;
        items(&self.items, &request.context, service)
    }
}
impl ServiceDescription {
    pub fn validate(&self) -> Result<()> {
        id(&self.identity)?;
        require(self.version == 1 && self.capabilities.len() <= 11)?;
        let mut operations = HashSet::new();
        for capability in &self.capabilities {
            require(operations.insert(capability.operation))?;
            require(capability.forget_targets.len() <= 3 && capability.forget_levels.len() <= 4)?;
            unique(&capability.forget_targets)?;
            unique(&capability.forget_levels)?;
            if capability.operation == Operation::Forget {
                require(
                    !capability.forget_targets.is_empty() && !capability.forget_levels.is_empty(),
                )?;
            } else {
                require(
                    capability.forget_targets.is_empty() && capability.forget_levels.is_empty(),
                )?;
            }
            unique(&capability.effects)?;
            require(capability.effects.len() <= 3)?;
            if matches!(
                capability.operation,
                Operation::Recall | Operation::Inspect | Operation::OperationStatus
            ) {
                require(capability.effects.is_empty())?;
            }
            if capability.operation == Operation::Prepare {
                require(
                    capability
                        .effects
                        .iter()
                        .all(|effect| *effect == Effect::StoreEvidence),
                )?;
            }
        }
        require(operations.contains(&Operation::Recall))
    }
    pub fn capability(&self, operation: Operation) -> Result<&Capability> {
        self.capabilities
            .iter()
            .find(|capability| capability.operation == operation)
            .ok_or_else(|| Error::new(ErrorKind::Unsupported))
    }
}
impl LearningGrant {
    pub fn validate(&self, context: &MemoryContext) -> Result<()> {
        context.validate()?;
        require(!self.scopes.is_empty() && self.scopes.len() <= MAX_SCOPES)?;
        unique(&self.scopes)?;
        for scope in &self.scopes {
            context.validate_scope(scope)?;
        }
        require(!self.input_classes.is_empty() && self.input_classes.len() <= MAX_ITEMS)?;
        unique(&self.input_classes)?;
        for class in &self.input_classes {
            require(valid_opaque_id(class, 64))?;
        }
        Ok(())
    }
}
impl Observation {
    pub fn validate(&self) -> Result<()> {
        id(&self.event_id)?;
        self.content.validate()?;
        self.source.validate()
    }
}
fn request(context: &MemoryContext, request_id: &str) -> Result<()> {
    context.validate()?;
    id(request_id)
}
impl ObserveRequest {
    pub fn validate(&self) -> Result<()> {
        request(&self.context, &self.request_id)?;
        require(!self.observations.is_empty())?;
        if self.mode == ObserveMode::Learn {
            require(self.context.write_scope.is_some())?;
        }
        observations(&self.observations)
    }
}
impl RememberRequest {
    pub fn validate(&self, service: &str) -> Result<()> {
        require(self.key.as_deref().is_none_or(valid_memory_key))?;
        request(&self.context, &self.request_id)?;
        self.content.validate()?;
        if let Some(target) = &self.target {
            target.validate(&self.context, service)?;
        } else {
            require(self.context.write_scope.is_some())?;
        }
        metadata(&self.metadata)
    }
}
impl ReflectRequest {
    pub fn validate(&self) -> Result<()> {
        request(&self.context, &self.request_id)?;
        require(!self.evidence.is_empty() || !self.sources.is_empty())?;
        require(self.context.write_scope.is_some())?;
        observations(&self.evidence)?;
        sources(&self.sources)?;
        text(&self.focus, MAX_QUERY_BYTES, true)
    }
}
impl ForgetRequest {
    pub fn validate(&self, service: &str) -> Result<()> {
        request(&self.context, &self.request_id)?;
        match &self.target {
            ForgetTarget::Item(target) => target.validate(&self.context, service),
            ForgetTarget::Source(source) => source.validate(),
            ForgetTarget::Scope(scope) => self.context.validate_scope(scope),
        }
    }
}
impl InspectRequest {
    pub fn validate(&self, service: &str) -> Result<()> {
        self.context.validate()?;
        match &self.query {
            InspectQuery::Get(reference) => reference.validate(&self.context, service),
            InspectQuery::List {
                scope,
                limit,
                cursor,
            } => {
                self.context.validate_scope(scope)?;
                require(*limit > 0 && *limit <= MAX_ITEMS)?;
                require(cursor.is_empty() || valid_opaque_id(cursor, MAX_CURSOR_BYTES))?;
                Ok(())
            }
            InspectQuery::Status => Ok(()),
        }
    }
}
impl InspectResult {
    pub fn validate(&self, request: &InspectRequest, service: &str) -> Result<()> {
        request.validate(service)?;
        items(&self.items, &request.context, service)?;
        match &request.query {
            InspectQuery::Get(reference) => {
                require(self.items.len() <= 1)?;
                for item in &self.items {
                    require(item.reference.as_ref().is_some_and(|returned| {
                        returned.service == reference.service
                            && returned.scope == reference.scope
                            && returned.id == reference.id
                            && reference
                                .revision
                                .as_ref()
                                .is_none_or(|revision| returned.revision.as_ref() == Some(revision))
                    }))?;
                }
            }
            InspectQuery::List { scope, limit, .. } => {
                require(self.items.len() <= *limit)?;
                for item in &self.items {
                    require(item.reference.as_ref().is_some_and(|r| &r.scope == scope))?;
                }
            }
            InspectQuery::Status => require(self.items.is_empty())?,
        }
        require(
            self.next_cursor.is_empty() || valid_opaque_id(&self.next_cursor, MAX_CURSOR_BYTES),
        )?;
        text(&self.state, MAX_CONTENT_BYTES, true)
    }
}
impl FeedbackRequest {
    pub fn validate(&self, service: &str) -> Result<()> {
        request(&self.context, &self.request_id)?;
        require(!self.references.is_empty())?;
        count(&self.references)?;
        for reference in &self.references {
            reference.validate(&self.context, service)?;
        }
        Ok(())
    }
}
impl MutatingRequest {
    pub fn context(&self) -> &MemoryContext {
        match self {
            Self::Observe(r) => &r.context,
            Self::Remember(r) => &r.context,
            Self::Reflect(r) => &r.context,
            Self::Forget(r) => &r.context,
            Self::Feedback(r) => &r.context,
        }
    }
    pub fn operation(&self) -> Operation {
        match self {
            Self::Observe(_) => Operation::Observe,
            Self::Remember(_) => Operation::Remember,
            Self::Reflect(_) => Operation::Reflect,
            Self::Forget(_) => Operation::Forget,
            Self::Feedback(_) => Operation::Feedback,
        }
    }
    pub fn validate(&self, service: &str) -> Result<()> {
        match self {
            Self::Observe(r) => r.validate(),
            Self::Remember(r) => r.validate(service),
            Self::Reflect(r) => r.validate(),
            Self::Forget(r) => r.validate(service),
            Self::Feedback(r) => r.validate(service),
        }
    }
}
impl PrepareRequest {
    pub fn validate(&self, service: &str) -> Result<()> {
        self.operation.validate(service)
    }
}
impl PreparedChanges {
    pub fn validate(&self, request: &PrepareRequest, service: &str) -> Result<()> {
        request.validate(service)?;
        require(&self.context == request.operation.context())?;
        self.validate_snapshot(request.operation.context(), service)
    }

    /// Checks a persisted projection against current host-established
    /// authority. A human applying a model preparation may have a different
    /// origin or session; that transition must be authorized by the host.
    /// This validates data binding, not approval or snapshot authenticity.
    pub fn validate_snapshot(&self, context: &MemoryContext, service: &str) -> Result<()> {
        context.validate()?;
        self.context.validate()?;
        id(service)?;
        id(&self.reference)?;
        if self.context.principal != context.principal {
            return Err(Error::new(ErrorKind::Unauthorized));
        }
        for scope in &self.context.scopes {
            context.validate_scope(scope)?;
        }
        count(&self.changes)?;
        unique(&self.effects)?;
        require(self.effects.len() <= 3)?;
        require(self.changes.is_empty() || self.effects.contains(&Effect::ChangeMemory))?;
        let mut bytes = 0;
        for change in &self.changes {
            self.context.validate_scope(&change.scope)?;
            context.validate_scope(&change.scope)?;
            sources(&change.sources)?;
            projection_metadata(&change.metadata)?;
            if let Some(target) = &change.target {
                target.validate(&self.context, service)?;
                require(target.scope == change.scope)?;
            }
            match change.action {
                ChangeAction::Create => {
                    require(change.target.is_none() && change.content.is_some())?
                }
                ChangeAction::Update => {
                    require(change.target.is_some() && change.content.is_some())?
                }
                ChangeAction::Forget => {
                    require(change.target.is_some() && change.content.is_none())?
                }
            }
            if let Some(content) = &change.content {
                content.validate()?;
                bytes += content.byte_len();
            }
        }
        require(bytes <= MAX_BATCH_BYTES)
    }
}
impl ApplyRequest {
    pub fn validate(&self) -> Result<()> {
        request(&self.context, &self.request_id)?;
        id(&self.preparation)
    }
}
impl OperationStatusRequest {
    pub fn validate(&self) -> Result<()> {
        self.context.validate()?;
        id(&self.operation_ref)
    }
}
impl Receipt {
    /// A successful submission is not proof of the requested forgetting
    /// guarantee. Levels are exact declarations, not an inferred hierarchy.
    pub fn validate_forget(&self, request: &ForgetRequest, service: &str) -> Result<()> {
        request.validate(service)?;
        self.validate(&request.context, &request.request_id, service)?;
        if self.completion == Completion::Complete {
            require(
                self.forget_coverage
                    .as_ref()
                    .is_some_and(|coverage| coverage.achieved.contains(&request.coverage)),
            )?;
        }
        Ok(())
    }

    pub fn validate(&self, context: &MemoryContext, request_id: &str, service: &str) -> Result<()> {
        context.validate()?;
        id(&self.request_id)?;
        require(self.request_id == request_id)?;
        optional_id(&self.operation_ref)?;
        count(&self.items)?;
        for outcome in &self.items {
            if let Some(reference) = &outcome.reference {
                reference.validate(context, service)?;
            }
        }
        if let Some(coverage) = &self.forget_coverage {
            unique(&coverage.achieved)?;
            unique(&coverage.remaining)?;
            require(coverage.achieved.len() <= 4 && coverage.remaining.len() <= 4)?;
            require(
                !coverage
                    .achieved
                    .iter()
                    .any(|level| coverage.remaining.contains(level)),
            )?;
        }
        if self.admission == Admission::Rejected {
            require(!matches!(
                self.completion,
                Completion::Pending
                    | Completion::Running
                    | Completion::Complete
                    | Completion::Partial
            ))?;
            require(
                !self
                    .items
                    .iter()
                    .any(|item| item.state == ItemState::Applied),
            )?;
        }
        if self.completion == Completion::Complete {
            require(
                !self
                    .items
                    .iter()
                    .any(|item| matches!(item.state, ItemState::Failed | ItemState::Unknown)),
            )?;
        }
        Ok(())
    }
}

/// Stable semantic slot; punctuation separates nonempty lower-case alphanumeric segments.
pub fn valid_memory_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key.split(['.', '_', '-']).all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}
