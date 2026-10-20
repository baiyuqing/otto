//! The `MemoryService` implementation for the remote service.

use super::*;

#[async_trait::async_trait]
impl MemoryService for RemoteService {
    fn describe(&self) -> ServiceDescription {
        let capability = |operation, effects: Vec<Effect>| Capability {
            effects,
            supports_delegated_learning: true,
            ..Capability::new(operation)
        };
        let mut remember = capability(Operation::Remember, vec![Effect::ChangeMemory]);
        remember.conditional_updates = true;
        let mut forget = capability(Operation::Forget, vec![Effect::ChangeMemory]);
        forget.forget_targets = vec![ForgetTargetKind::Item];
        forget.forget_levels = vec![ForgetLevel::Suppress];
        ServiceDescription {
            identity: IDENTITY.into(),
            version: 1,
            capabilities: vec![
                capability(Operation::Recall, Vec::new()),
                capability(Operation::Inspect, Vec::new()),
                remember,
                forget,
            ],
            encryption_at_rest: None,
        }
    }

    async fn recall(
        &self,
        request: &RecallRequest,
        control: &dyn OperationControl,
    ) -> Result<RecallResult> {
        request.validate()?;
        let context = Self::context(&request.context)?;
        let body = wire::RecallBody {
            context: context.clone(),
            query: request.query.clone(),
            scope_filter: request.scope_filter.clone(),
            limit: request.limit,
            max_bytes: request.token_budget,
        };
        let response: wire::RecallResponse = self
            .call("recall", &body, self.recall_timeout, false, control)
            .await?;
        let rejected = || err(ErrorKind::Unavailable);
        if response.context != context
            || response.coverage != wire::Coverage::Complete
            || response.items.len() > request.limit
        {
            return Err(rejected());
        }
        let mut items = Vec::with_capacity(response.items.len());
        let mut refs = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        let mut last_group = true;
        let mut last_priority = u8::MAX;
        for recalled in response.items {
            let item = recalled.item;
            let priority = item.scope.namespace.priority();
            if request
                .scope_filter
                .as_ref()
                .is_some_and(|filter| !filter.contains(&item.scope.namespace))
                || !refs.insert((item.scope.clone(), item.id.clone()))
                || item
                    .key
                    .as_ref()
                    .is_some_and(|key| !keys.insert(key.clone()))
                || (!last_group && recalled.mandatory)
                || (last_group == recalled.mandatory && priority > last_priority)
                || (item.pinned && !recalled.mandatory)
            {
                return Err(rejected());
            }
            last_group = recalled.mandatory;
            last_priority = priority;
            items.push(
                Self::item(
                    item,
                    &request.context,
                    recalled.mandatory,
                    MemoryView::Effective,
                )
                .ok_or_else(rejected)?,
            );
        }
        let records: Vec<_> = items.iter().map(record).collect();
        let used = render_memory_context(&records).len();
        if used > request.token_budget {
            return Err(rejected());
        }
        Ok(RecallResult {
            items,
            used_tokens: Some(used),
            coverage: Coverage::Complete,
        })
    }

    async fn inspect(
        &self,
        request: &InspectRequest,
        control: &dyn OperationControl,
    ) -> Result<InspectResult> {
        let InspectQuery::Get(reference) = &request.query else {
            return Err(err(ErrorKind::Unsupported));
        };
        let context = Self::context(&request.context)?;
        request.validate(IDENTITY)?;
        let body = serde_json::to_vec(&wire::GetBody {
            context,
            scope: (&reference.scope).into(),
            id: reference.id.clone(),
        })
        .map_err(|_| err(ErrorKind::InvalidInput))?;
        let raw = self
            .exchange("get", body, self.recall_timeout, false, control)
            .await?;
        let mut result = InspectResult {
            items: Vec::new(),
            next_cursor: String::new(),
            state: "raw".into(),
        };
        if raw.status == 404 {
            if !empty_object(&raw.body) {
                return Err(err(ErrorKind::Unavailable));
            }
            return Ok(result);
        }
        let raw = raw.into_success(self, "get")?;
        let invalid = || err(ErrorKind::Unavailable);
        if raw.status != 200 {
            return Err(invalid());
        }
        let response: wire::GetResponse = wire::decode(&raw.body).map_err(|_| invalid())?;
        if response.organization_id
            != request
                .context
                .organization_id
                .as_deref()
                .unwrap_or_default()
            || response.item.id != reference.id
            || Scope::from(response.item.scope.clone()) != reference.scope
        {
            return Err(invalid());
        }
        result.items.push(
            Self::item(response.item, &request.context, false, MemoryView::Raw)
                .ok_or_else(invalid)?,
        );
        Ok(result)
    }

    async fn remember(
        &self,
        request: &RememberRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        request.validate(IDENTITY)?;
        let context = Self::context(&request.context)?;
        if !valid_request_id(&request.request_id) {
            return Err(err(ErrorKind::InvalidInput));
        }
        if let Some(target) = &request.target {
            check_target(&request.context, target)?;
        }
        let Content::Text(text) = &request.content else {
            return Err(err(ErrorKind::InvalidInput));
        };
        let scope = match &request.target {
            Some(target) => target.scope.clone(),
            None => request
                .context
                .write_scope
                .clone()
                .ok_or_else(|| err(ErrorKind::InvalidInput))?,
        };
        request.context.validate_scope(&scope)?;
        if scope.namespace == "organization" && request.context.origin != Origin::Human {
            return Err(err(ErrorKind::Unauthorized));
        }
        let body = wire::RememberBody {
            context,
            key: request.key.clone(),
            request_id: request.request_id.clone(),
            origin: Self::origin(&request.context)?,
            scope: (&scope).into(),
            text: text.clone(),
            target: request.target.as_ref().map(|target| wire::Target {
                id: target.id.clone(),
                revision: target.revision.clone().expect("validated revision"),
            }),
            metadata: request.metadata.clone(),
        };
        let receipt: wire::Receipt = self
            .call("remember", &body, self.write_timeout, true, control)
            .await?;
        self.receipt(
            "remember",
            &request.request_id,
            &scope,
            receipt,
            &request.context,
            request.target.as_ref(),
        )
    }

    async fn forget(
        &self,
        request: &ForgetRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        let ForgetTarget::Item(target) = &request.target else {
            return Err(err(ErrorKind::Unsupported));
        };
        if request.coverage != ForgetLevel::Suppress {
            return Err(err(ErrorKind::Unsupported));
        }
        request.validate(IDENTITY)?;
        let context = Self::context(&request.context)?;
        check_target(&request.context, target)?;
        if !valid_request_id(&request.request_id) {
            return Err(err(ErrorKind::InvalidInput));
        }
        if target.scope.namespace == "organization" {
            return Err(err(ErrorKind::Unauthorized));
        }
        let body = wire::ForgetBody {
            context,
            request_id: request.request_id.clone(),
            origin: Self::origin(&request.context)?,
            scope: (&target.scope).into(),
            id: target.id.clone(),
            revision: target.revision.clone().expect("validated revision"),
        };
        let receipt: wire::Receipt = self
            .call("forget", &body, self.write_timeout, true, control)
            .await?;
        self.receipt(
            "forget",
            &request.request_id,
            &target.scope,
            receipt,
            &request.context,
            Some(target),
        )
    }
}
