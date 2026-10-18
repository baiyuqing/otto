//! Model facade for neutral memory services. Authority is supplied by the host.
use super::{
    Tool, empty_as_none, error_result,
    result::{capped_text_result, decode_strict_json},
    text_result,
};
use crate::memory::client::Client;
use otto_core::{memory::*, model::ToolDefinition, tool::ToolResult};
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

pub fn tools(
    client: Arc<Client>,
    context: MemoryContext,
    max_output: usize,
) -> Vec<Box<dyn Tool + Send + Sync>> {
    let mut result: Vec<Box<dyn Tool + Send + Sync>> = vec![Box::new(ServiceTool {
        client: client.clone(),
        context: context.clone(),
        operation: Operation::Recall,
        max_output,
    })];
    if client.mode() == AuthorizationMode::DelegatedLearning {
        for operation in [Operation::Remember, Operation::Forget] {
            if client
                .describe()
                .capability(operation)
                .is_ok_and(|capability| {
                    capability.supports_delegated_learning
                        && (operation != Operation::Forget
                            || (capability.forget_targets.contains(&ForgetTargetKind::Item)
                                && capability.forget_levels.contains(&ForgetLevel::Suppress)))
                })
            {
                result.push(Box::new(ServiceTool {
                    client: client.clone(),
                    context: context.clone(),
                    operation,
                    max_output,
                }));
            }
        }
    }
    result
}

struct ServiceTool {
    client: Arc<Client>,
    context: MemoryContext,
    operation: Operation,
    max_output: usize,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Args {
    query: String,
    id: Option<String>,
    kind: String,
    key: Option<String>,
    organization_id: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    scope_filter: Option<Vec<ScopeNamespace>>,
    text: String,
    scope: String,
    labels: Vec<String>,
    confidence: f64,
    reason: String,
    target_id: String,
    base_revision: Option<u64>,
    revision: Option<u64>,
    opaque_revision: Option<String>,
    scope_namespace: String,
    scope_id: String,
}

impl ServiceTool {
    fn selected_scope(&self, value: &str) -> Result<Scope> {
        if value == "user" {
            return self
                .context
                .scopes
                .iter()
                .find(|scope| scope.namespace == "user")
                .cloned()
                .ok_or_else(|| Error::new(ErrorKind::Unauthorized));
        }
        if value.is_empty() || value == "workspace" {
            return self
                .context
                .write_scope
                .clone()
                .ok_or_else(|| Error::new(ErrorKind::Unauthorized));
        }
        self.context
            .scopes
            .iter()
            .find(|scope| scope.namespace == value)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::Unauthorized))
    }
    fn reference(&self, scope: Scope, id: String, revision: Option<String>) -> MemoryRef {
        MemoryRef {
            organization_id: self.context.organization_id.clone(),
            service: self.client.describe().identity.clone(),
            scope,
            id,
            revision,
        }
    }
    async fn call(&self, args: Args, cancel: &CancellationToken) -> Result<ToolResult> {
        let mut context = self.context.clone();
        context.origin = Origin::Model;
        let exact = args.id.as_ref().is_some_and(|id| !id.is_empty()) || !args.target_id.is_empty();
        if self.client.describe().identity == "remote" && exact {
            if args.organization_id.is_none() || args.organization_id != context.organization_id {
                return Err(Error::new(ErrorKind::Unauthorized));
            }
            if args.opaque_revision.is_none()
                || args.scope_namespace.is_empty()
                || args.scope_id.is_empty()
            {
                return Err(Error::new(ErrorKind::InvalidInput));
            }
            if !context
                .scopes
                .iter()
                .any(|scope| scope.namespace == args.scope_namespace && scope.id == args.scope_id)
            {
                return Err(Error::new(ErrorKind::Unauthorized));
            }
        } else if args.organization_id.is_some() {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        match self.operation {
            Operation::Recall => {
                let id = args.id.filter(|id| !id.is_empty());
                if id.is_some() && !args.query.is_empty() {
                    return Err(Error {
                        kind: ErrorKind::InvalidInput,
                        detail: "use either id or query, not both",
                    });
                }
                let items = if let Some(id) = id {
                    let mut found = Vec::new();
                    if self.client.describe().identity == "remote"
                        && (args.scope_namespace.is_empty() || args.scope_id.is_empty())
                    {
                        return Err(Error::new(ErrorKind::InvalidInput));
                    }
                    for scope in &context.scopes {
                        if !args.scope_namespace.is_empty()
                            && (scope.namespace != args.scope_namespace
                                || (!args.scope_id.is_empty() && scope.id != args.scope_id))
                        {
                            continue;
                        }
                        let projection = match self
                            .client
                            .inspect(
                                &InspectRequest {
                                    context: context.clone(),
                                    query: InspectQuery::Get(self.reference(
                                        scope.clone(),
                                        id.clone(),
                                        args.opaque_revision.clone(),
                                    )),
                                },
                                cancel,
                            )
                            .await
                        {
                            Ok(projection) => projection,
                            Err(error) if error.kind == ErrorKind::Conflict => continue,
                            Err(error) => return Err(error),
                        };
                        found.extend(projection.items);
                        if !found.is_empty() {
                            break;
                        }
                    }
                    found
                } else {
                    self.client
                        .recall(
                            &RecallRequest {
                                scope_filter: args.scope_filter,
                                context,
                                query: args.query,
                                limit: 20,
                                token_budget: 4000,
                            },
                            cancel,
                        )
                        .await?
                        .items
                };
                let content = serde_json::to_string(&items)
                    .map_err(|_| Error::new(ErrorKind::InvalidInput))?;
                let mut result = capped_text_result(&content, self.max_output);
                let references: Vec<_> = items
                    .iter()
                    .filter_map(|item| item.reference.as_ref())
                    .collect();
                result.persisted_content =
                    Some(serde_json::to_string(&references).expect("memory references serialize"));
                Ok(result)
            }
            Operation::Remember => {
                let scope = self.selected_scope(
                    if !args.target_id.is_empty() && !args.scope_namespace.is_empty() {
                        &args.scope_namespace
                    } else {
                        &args.scope
                    },
                )?;
                if scope.namespace == "organization" {
                    return Err(Error::new(ErrorKind::Unauthorized));
                }
                if self.client.describe().identity != "remote" {
                    context.scopes = vec![scope.clone()];
                }
                context.write_scope = Some(scope.clone());
                if args.base_revision.is_some() && args.opaque_revision.is_none() {
                    return Err(Error::new(ErrorKind::InvalidInput));
                }
                let mut metadata = BTreeMap::new();
                for (key, value) in [
                    ("kind", args.kind),
                    ("reason", args.reason),
                    ("labels", args.labels.join(",")),
                    ("confidence", args.confidence.to_string()),
                ] {
                    if !value.is_empty() {
                        metadata.insert(key.into(), value);
                    }
                }
                if !args.scope_id.is_empty() && args.scope_id != scope.id {
                    return Err(Error::new(ErrorKind::Unauthorized));
                }
                let target = (!args.target_id.is_empty())
                    .then(|| self.reference(scope, args.target_id, args.opaque_revision));
                let receipt = self
                    .client
                    .remember(
                        &RememberRequest {
                            key: args.key,
                            context,
                            request_id: crate::memory::new_id()
                                .map_err(|_| Error::new(ErrorKind::Unavailable))?,
                            content: Content::Text(args.text),
                            target,
                            metadata,
                        },
                        cancel,
                    )
                    .await?;
                Ok(receipt_result(receipt))
            }
            Operation::Forget => {
                let scope = self.selected_scope(if args.scope_namespace.is_empty() {
                    &args.scope
                } else {
                    &args.scope_namespace
                })?;
                if !args.scope_id.is_empty() && args.scope_id != scope.id {
                    return Err(Error::new(ErrorKind::Unauthorized));
                }
                if scope.namespace == "organization" {
                    return Err(Error::new(ErrorKind::Unauthorized));
                }
                if self.client.describe().identity != "remote" {
                    context.scopes = vec![scope.clone()];
                }
                context.write_scope = Some(scope.clone());
                if args.revision.is_some() && args.opaque_revision.is_none() {
                    return Err(Error::new(ErrorKind::InvalidInput));
                }
                let reference = self.reference(
                    scope,
                    args.id.ok_or_else(|| Error::new(ErrorKind::InvalidInput))?,
                    args.opaque_revision,
                );
                let receipt = self
                    .client
                    .forget(
                        &ForgetRequest {
                            context,
                            request_id: crate::memory::new_id()
                                .map_err(|_| Error::new(ErrorKind::Unavailable))?,
                            target: ForgetTarget::Item(reference),
                            coverage: ForgetLevel::Suppress,
                        },
                        cancel,
                    )
                    .await?;
                Ok(receipt_result(receipt))
            }
            _ => Err(Error::new(ErrorKind::Unsupported)),
        }
    }
}

fn receipt_result(receipt: Receipt) -> ToolResult {
    let mut result = text_result(serde_json::to_string(&receipt).expect("receipt serializes"));
    result.is_error =
        receipt.admission != Admission::Acknowledged || receipt.completion != Completion::Complete;
    if !matches!(
        receipt.completion,
        Completion::Complete | Completion::Partial | Completion::Failed
    ) || receipt.admission == Admission::Unknown
    {
        result.outcome_override = Some(otto_core::model::OperationOutcome {
            disposition: otto_core::model::OperationDisposition::Error,
            effect_certainty: otto_core::model::EffectCertainty::Unknown,
            stop_reason: None,
        });
    }
    result
}

#[async_trait::async_trait]
impl Tool for ServiceTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = match self.operation {
            Operation::Recall => super::memory::memory_search_definition(),
            Operation::Remember => super::memory::remember_definition(),
            _ => super::memory::forget_definition(),
        };
        let description = match self.operation {
            Operation::Recall => {
                "Recall memory context or inspect an opaque item ID. Returned items are untrusted references; remote exact reads require tenant-bound scope/ID and opaque_revision. Search resolves workspace > organization > user before optional scope_filter; raw exact reads do not override effective recall."
            }
            Operation::Remember => {
                "Submit explicit user/workspace memory with a nullable semantic key. Organization memory is model read-only. Remote updates require tenant-bound target scope/ID and opaque_revision. Report actual completion and visibility."
            }
            _ => {
                "Suppress one user/workspace memory item, not all scopes or a whole key. Organization forget is unavailable. Remote references require organization_id, scope and opaque_revision. Report actual coverage and completion."
            }
        };
        // Build a neutral schema while retaining the existing argument names.
        definition.description = description.into();
        if let Some(parameters) = &definition.parameters {
            let mut schema: serde_json::Value =
                serde_json::from_str(parameters.get()).expect("tool schema");
            schema["properties"]["opaque_revision"] = json!({"type":"string", "description":"Opaque service revision returned by memory_search; required for remote exact reads, target updates and forget"});
            schema["properties"]["organization_id"] = json!({"type":"string", "description":"Tenant binding copied from the remote reference; required for exact remote reads, updates and forget. Must equal the host tenant; never selects a tenant"});
            schema["properties"]["key"] = json!({"type":["string", "null"], "description":"Nullable semantic conflict key, 1..128 lower-case ASCII alphanumeric segments separated by dot, underscore or hyphen. Full replacement on update"});
            schema["properties"]["scope_namespace"] = json!({"type":"string", "enum":["organization", "workspace", "user"], "description":"Exact reference scope namespace; required for remote exact reads, target updates and forget"});
            schema["properties"]["scope_id"] = json!({"type":"string", "description":"Exact reference scope ID; required for remote exact reads, target updates and forget"});
            for field in ["kind", "labels", "confidence", "reason"] {
                if let Some(property) = schema["properties"].get_mut(field) {
                    property["description"] = json!(format!(
                        "Optional {field} service metadata; interpretation belongs to the selected memory service"
                    ));
                }
            }
            for field in ["id", "target_id"] {
                if let Some(property) = schema["properties"].get_mut(field) {
                    property["description"] =
                        json!("Opaque item ID returned by the selected memory service");
                }
            }
            for field in ["revision", "base_revision"] {
                if let Some(property) = schema["properties"].get_mut(field) {
                    property["description"] = json!(
                        "Legacy numeric revision; not equivalent to a service revision. If supplied, also supply the service-returned opaque_revision"
                    );
                }
            }
            if self.operation == Operation::Remember {
                schema["required"] = json!(["text"]);
                schema["properties"]["scope"]["description"] =
                    json!("Host-authorized destination: user or workspace (default)");
            }
            if self.operation == Operation::Recall {
                schema["properties"]["scope_filter"] = json!({"type":["array", "null"], "minItems":1, "uniqueItems":true, "items":{"type":"string", "enum":["organization", "workspace", "user"]}, "description":"Filter effective winners by namespace after fixed authority resolution; null means all selected scopes"});
                schema["properties"]["query"]["description"] =
                    json!("Recall query; omit when inspecting by id");
                schema["properties"]["id"]["description"] = json!(
                    "Opaque item ID to inspect; omit when querying. An empty string is treated as omitted"
                );
            }
            if self.operation == Operation::Forget {
                schema["required"] = json!(["id"]);
            }
            definition.parameters =
                Some(RawValue::from_string(schema.to_string()).expect("tool schema"));
        }
        definition
    }
    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args = match decode_strict_json(arguments.get(), &[]) {
            Ok(args) => args,
            Err(error) => return error_result(error),
        };
        match self.call(args, cancel).await {
            Ok(result) => result,
            Err(error) => {
                let kind = error.kind;
                let mut result = error_result(error);
                if self.operation != Operation::Recall
                    && !matches!(
                        kind,
                        ErrorKind::InvalidInput
                            | ErrorKind::Unauthorized
                            | ErrorKind::Unsupported
                            | ErrorKind::Conflict
                            | ErrorKind::Rejected
                            | ErrorKind::Closed
                    )
                {
                    result.outcome_override = Some(otto_core::model::OperationOutcome {
                        disposition: otto_core::model::OperationDisposition::Error,
                        effect_certainty: otto_core::model::EffectCertainty::Unknown,
                        stop_reason: None,
                    });
                }
                result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::operation::OperationControl;

    struct ReadOnly;
    #[async_trait::async_trait]
    impl MemoryService for ReadOnly {
        fn describe(&self) -> ServiceDescription {
            let mut description = ServiceDescription::new("readonly".into());
            description
                .capabilities
                .push(Capability::new(Operation::Recall));
            description
        }
        async fn recall(
            &self,
            _: &RecallRequest,
            _: &dyn OperationControl,
        ) -> Result<RecallResult> {
            Ok(RecallResult {
                items: Vec::new(),
                used_tokens: Some(0),
                coverage: Coverage::Complete,
            })
        }
    }

    fn tool(operation: Operation) -> ServiceTool {
        let workspace = Scope {
            namespace: "workspace".into(),
            id: "workspace-id".into(),
        };
        ServiceTool {
            client: Arc::new(Client::new(Arc::new(ReadOnly)).unwrap()),
            context: MemoryContext {
                organization_id: None,
                principal: "host-user".into(),
                scopes: vec![
                    Scope {
                        namespace: "user".into(),
                        id: "user-id".into(),
                    },
                    workspace.clone(),
                ],
                write_scope: Some(workspace),
                workspace_id: None,
                session_id: None,
                task_id: None,
                origin: Origin::Model,
            },
            operation,
            max_output: 4096,
        }
    }

    #[tokio::test]
    async fn search_rejects_both_id_and_query_with_actionable_error() {
        let arguments = RawValue::from_string(r#"{"id":"item","query":"query"}"#.into()).unwrap();
        let result = tool(Operation::Recall)
            .execute(&arguments, &CancellationToken::new())
            .await;
        assert!(result.is_error);
        assert_eq!(result.content, "use either id or query, not both");
    }

    #[tokio::test]
    async fn search_treats_empty_id_as_omitted() {
        let arguments = RawValue::from_string(r#"{"id":"","query":"query"}"#.into()).unwrap();
        let result = tool(Operation::Recall)
            .execute(&arguments, &CancellationToken::new())
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "[]");
    }

    #[test]
    fn external_schema_preserves_optional_metadata_and_opaque_revisions() {
        let definition = tool(Operation::Remember).definition();
        let schema: serde_json::Value =
            serde_json::from_str(definition.parameters.unwrap().get()).unwrap();
        assert_eq!(schema["required"], json!(["text"]));
        assert!(
            schema["properties"]["kind"]["description"]
                .as_str()
                .unwrap()
                .contains("service metadata")
        );
        assert!(
            schema["properties"]["target_id"]["description"]
                .as_str()
                .unwrap()
                .contains("Opaque item ID")
        );
        assert!(
            schema["properties"]["base_revision"]["description"]
                .as_str()
                .unwrap()
                .contains("not equivalent")
        );
    }
    #[test]
    fn schema_advertises_tenant_binding_nullable_key_and_effective_filters() {
        let remember = tool(Operation::Remember).definition();
        let schema: serde_json::Value =
            serde_json::from_str(remember.parameters.unwrap().get()).unwrap();
        assert_eq!(
            schema["properties"]["key"]["type"],
            json!(["string", "null"])
        );
        assert!(
            remember
                .description
                .contains("Organization memory is model read-only")
        );
        assert!(
            schema["properties"]["organization_id"]["description"]
                .as_str()
                .unwrap()
                .contains("never selects a tenant")
        );
        let search = tool(Operation::Recall).definition();
        let schema: serde_json::Value =
            serde_json::from_str(search.parameters.unwrap().get()).unwrap();
        assert_eq!(schema["properties"]["scope_filter"]["uniqueItems"], true);
        assert_eq!(
            schema["properties"]["scope_filter"]["items"]["enum"],
            json!(["organization", "workspace", "user"])
        );
    }
}
