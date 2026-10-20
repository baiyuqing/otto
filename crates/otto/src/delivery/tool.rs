//! The `start_delivery_task` model tool.

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolArgs {
    target: String,
    objective: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    source_refs: Vec<String>,
}

impl TryFrom<ToolArgs> for Request {
    type Error = String;

    fn try_from(value: ToolArgs) -> Result<Self, Self::Error> {
        if value.target.is_empty() || !Path::new(&value.target).is_absolute() {
            return Err("target must be an absolute path".into());
        }
        if value.objective.trim().is_empty() || value.objective.len() > MAX_OBJECTIVE_BYTES {
            return Err("objective must be between 1 and 16384 bytes".into());
        }
        if value.source_refs.len() > MAX_SOURCE_REFS
            || value.source_refs.iter().any(|item| {
                item.is_empty() || item.len() > MAX_SOURCE_REF_BYTES || item.contains('\0')
            })
        {
            return Err("source_refs must contain at most 32 bounded references".into());
        }
        Ok(Self {
            target: value.target,
            objective: value.objective,
            branch: value.branch,
            source_refs: value.source_refs,
        })
    }
}

pub fn start_definition() -> ToolDefinition {
    definition(
        TOOL_NAME,
        "Prepare a separately authorized delivery task in another admitted Git \
            repository. The source workspace stays read-only; a future target \
            worktree becomes the task's only writable root. This always requires \
            user authorization and does not grant GitHub, network, or unsandboxed \
            Bash access.",
        json!({
            "type":"object", "additionalProperties":false,
            "properties": {
                "target":{"type":"string","description":"Absolute path to an admitted primary Git checkout."},
                "objective":{"type":"string","description":"The delivery objective for the isolated target task."},
                "branch":{"type":"string","description":"Optional new branch; defaults to agent/<objective slug>."},
                "source_refs":{"type":"array","items":{"type":"string"},"description":"Bounded references already selected from source context; not filesystem paths."}
            },
            "required":["target","objective"]
        }),
    )
}

pub struct StartDeliveryTask {
    source_workspace: String,
    service: Arc<dyn Service>,
    /// Required target-host-bound preview source. This is intentionally not
    /// optional: no delivery approval may fall back to synchronous or bare
    /// Git inspection from a source runner.
    preflight: Arc<dyn Preflight>,
    pub(super) receipts: std::sync::Mutex<std::collections::HashMap<String, Receipt>>,
}

impl StartDeliveryTask {
    pub fn new(
        source_workspace: String,
        service: Arc<dyn Service>,
        preflight: Arc<dyn Preflight>,
    ) -> Self {
        Self {
            source_workspace,
            service,
            preflight,
            receipts: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn request(&self, arguments: &RawValue) -> Result<Request, ToolResult> {
        decode_strict_json(arguments.get(), &["target", "objective"])
            .map_err(|error| error_result(&error).not_started())
            .and_then(|args: ToolArgs| {
                Request::try_from(args).map_err(|error| error_result(&error).not_started())
            })
    }

    fn spec(preview: &Preview) -> ApprovalSpec {
        let details = json!({
            "source_workspace": preview.source_workspace,
            "target_repository": preview.plan.target.repository,
            "target_common_dir": preview.plan.target.common_dir,
            "worktree": preview.plan.path,
            "branch": preview.plan.branch,
            "base": preview.base,
            "handoff_digest": preview.handoff.digest,
            "handoff_bytes": preview.handoff.bytes,
        })
        .to_string();
        ApprovalSpec {
            version: hash(&details),
            source: "delivery".into(),
            authority: otto_core::approval::Authority::Boundary,
            title: "Start a cross-workspace delivery task?".into(),
            reason: "Creates one target-local worktree after approval; GitHub push and PR permissions remain separate.".into(),
            details,
            persistent: false,
        }
    }
}

pub(crate) const START_DELIVERY_TASK_RETRY: (&str, otto_core::model::RetrySafety) = (
    "start_delivery_task",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait]
impl Tool for StartDeliveryTask {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        START_DELIVERY_TASK_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        start_definition()
    }

    fn approval_spec(&self, _arguments: &RawValue) -> Result<Option<ApprovalSpec>, ToolResult> {
        Err(
            error_result("start_delivery_task requires asynchronous target preflight")
                .not_started(),
        )
    }

    async fn approval_spec_async(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> Result<Option<ApprovalSpec>, ToolResult> {
        let request = self.request(arguments)?;
        let preview = self
            .preflight
            .preview(&self.source_workspace, &request, cancel)
            .await
            .map_err(|error| error_result(&error).not_started())?;
        Ok(Some(Self::spec(&preview)))
    }

    async fn apply_approval(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
        approval: &otto_core::approval::Request,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("delivery approval cancelled".into());
        }
        let request = self.request(arguments).map_err(|result| result.content)?;
        let preview = self
            .preflight
            .preview(&self.source_workspace, &request, cancel)
            .await?;
        let current = Self::spec(&preview);
        if current.details != approval.subject.details
            || current.version != approval.subject.version
        {
            return Err("delivery target changed after approval".into());
        }
        let receipt = self.service.prepare(&approval.id, &preview).await?;
        self.receipts
            .lock()
            .map_err(|_| "delivery receipt store unavailable")?
            .insert(approval.id.clone(), receipt);
        Ok(())
    }

    async fn execute(&self, _arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        error_result("start_delivery_task requires the approval-enabled tool registry")
            .not_started()
    }

    async fn execute_approved(
        &self,
        _call: ToolCall<'_>,
        cancel: &CancellationToken,
        approval: &otto_core::approval::Request,
    ) -> ToolResult {
        let receipt = match self.receipts.lock() {
            Ok(mut receipts) => receipts.remove(&approval.id),
            Err(_) => return error_result("delivery receipt store unavailable").not_started(),
        };
        let Some(receipt) = receipt else {
            return error_result("delivery approval has no prepared receipt").not_started();
        };
        match self.service.start(&receipt, cancel).await {
            Ok(message) => text_result(message),
            Err(error) => error_result(error),
        }
    }
}
