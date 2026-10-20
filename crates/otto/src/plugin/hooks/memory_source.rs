//! A trusted package's `memory` command exposed as an `otto_core::memory::MemoryService`.
//! Each operation runs the command once: one JSON request on stdin, one JSON reply on stdout.
use super::{CommandError, Package, Runtime};
use otto_core::{
    config::plugins::MemoryCommand,
    memory::{
        ApplyRequest, Error, ErrorKind, FeedbackRequest, ForgetRequest, InspectRequest,
        InspectResult, MemoryService, ObserveRequest, Operation, OperationStatusRequest,
        PrepareRequest, PreparedChanges, RecallRequest, RecallResult, Receipt, ReflectRequest,
        RememberRequest, Result, ServiceDescription,
    },
    operation::OperationControl,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Arc;

/// The reply of the memory command: exactly one of `result` or `error`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Result(Value),
    Error(ReplyError),
}
/// Only the kind is read; any other plugin text is rejected, not stored.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyError {
    kind: ErrorKind,
}

/// Operations whose effect may have happened when the command is interrupted.
fn is_effectful(operation: &str) -> bool {
    matches!(
        operation,
        "observe" | "remember" | "reflect" | "forget" | "feedback" | "prepare" | "apply"
    )
}

impl Runtime {
    /// Starts the memory service of package `package_name` and reads its description.
    pub async fn memory_source(
        self: &Arc<Self>,
        snapshot: &crate::plugin::Snapshot,
        package_name: &str,
        control: &dyn OperationControl,
    ) -> std::result::Result<PluginMemoryService, String> {
        let Some(installed) = snapshot.packages.iter().find(|p| p.name == package_name) else {
            return Err(format!(
                "memory plugin \"{package_name}\" is not installed or is disabled"
            ));
        };
        if !installed.trusted {
            return Err(format!(
                "memory plugin \"{package_name}\" is not trusted; run otto plugin trust {package_name}"
            ));
        }
        let Some(memory_command) = installed.package.otto.memory.clone() else {
            return Err(format!(
                "memory plugin \"{package_name}\" declares no memory component"
            ));
        };
        let Some(package) = self
            .packages
            .iter()
            .find(|p| p.installed.name == package_name)
        else {
            return Err(format!(
                "memory plugin \"{package_name}\" is not installed or is disabled"
            ));
        };
        let description = run_command::<ServiceDescription>(
            self,
            package,
            &memory_command,
            "describe",
            None,
            control,
        )
        .await
        .map_err(|error| {
            format!(
                "memory plugin \"{package_name}\" describe failed: {}",
                error.detail
            )
        })?;
        Ok(PluginMemoryService {
            runtime: self.clone(),
            package: package.clone(),
            memory_command,
            description,
        })
    }
}

/// Runs the memory command for `operation` and maps every failure to a safe `Error`.
async fn run_command<Res: DeserializeOwned>(
    runtime: &Runtime,
    package: &Package,
    memory_command: &MemoryCommand,
    operation: &str,
    request: Option<Value>,
    control: &dyn OperationControl,
) -> Result<Res> {
    let mut input = json!({"version": 1, "operation": operation});
    if let Some(request) = request {
        input["request"] = request;
    }
    let output = runtime
        .command(
            &memory_command.command,
            &memory_command.timeout,
            Some(package),
            input,
            control,
            None,
        )
        .await
        .map_err(|error: CommandError| {
            if error.interrupted && is_effectful(operation) {
                Error::new(ErrorKind::OutcomeUnknown)
            } else {
                Error::new(ErrorKind::Unavailable)
            }
        })?;
    match serde_json::from_str::<Reply>(&runtime.redact_json(&output)) {
        Ok(Reply::Result(value)) => {
            serde_json::from_value(value).map_err(|_| Error::new(ErrorKind::Unavailable))
        }
        Ok(Reply::Error(error)) => Err(Error::new(error.kind)),
        Err(_) => Err(Error::new(ErrorKind::Unavailable)),
    }
}

pub struct PluginMemoryService {
    runtime: Arc<Runtime>,
    package: Package,
    memory_command: MemoryCommand,
    description: ServiceDescription,
}

impl PluginMemoryService {
    async fn call<Req: Serialize, Res: DeserializeOwned>(
        &self,
        operation: Operation,
        request: &Req,
        control: &dyn OperationControl,
    ) -> Result<Res> {
        let name = serde_json::to_value(operation)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
        let request =
            serde_json::to_value(request).map_err(|_| Error::new(ErrorKind::InvalidInput))?;
        run_command(
            &self.runtime,
            &self.package,
            &self.memory_command,
            &name,
            Some(request),
            control,
        )
        .await
    }
}

#[async_trait::async_trait]
impl MemoryService for PluginMemoryService {
    fn describe(&self) -> ServiceDescription {
        self.description.clone()
    }
    async fn recall(
        &self,
        request: &RecallRequest,
        control: &dyn OperationControl,
    ) -> Result<RecallResult> {
        self.call(Operation::Recall, request, control).await
    }
    async fn observe(
        &self,
        request: &ObserveRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.call(Operation::Observe, request, control).await
    }
    async fn remember(
        &self,
        request: &RememberRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.call(Operation::Remember, request, control).await
    }
    async fn reflect(
        &self,
        request: &ReflectRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.call(Operation::Reflect, request, control).await
    }
    async fn forget(
        &self,
        request: &ForgetRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.call(Operation::Forget, request, control).await
    }
    async fn inspect(
        &self,
        request: &InspectRequest,
        control: &dyn OperationControl,
    ) -> Result<InspectResult> {
        self.call(Operation::Inspect, request, control).await
    }
    async fn feedback(
        &self,
        request: &FeedbackRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.call(Operation::Feedback, request, control).await
    }
    async fn prepare(
        &self,
        request: &PrepareRequest,
        control: &dyn OperationControl,
    ) -> Result<PreparedChanges> {
        self.call(Operation::Prepare, request, control).await
    }
    async fn apply(
        &self,
        request: &ApplyRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.call(Operation::Apply, request, control).await
    }
    async fn operation_status(
        &self,
        request: &OperationStatusRequest,
        control: &dyn OperationControl,
    ) -> Result<Receipt> {
        self.call(Operation::OperationStatus, request, control)
            .await
    }
}
