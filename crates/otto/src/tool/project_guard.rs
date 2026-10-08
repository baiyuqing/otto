//! Recheck executable authorization at the effect boundary, including existing sessions.
use super::{ApprovalSpec, Tool};
use otto_core::{
    model::ToolDefinition,
    tool::{ToolCall, ToolResult},
};
use serde_json::value::RawValue;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Guard {
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub user: PathBuf,
    pub check_trust: bool,
    pub expected_digest: Option<String>,
    pub require_sandbox: bool,
}

impl Guard {
    pub fn for_config(file: &otto_core::config::File, home: &str, workspace: &str) -> Option<Self> {
        let check_trust = file.project_executables == Some(true);
        let require_sandbox = file
            .policy
            .as_ref()
            .is_some_and(|p| p.require_sandbox == Some(true) || p.force.get("sandbox").is_some());
        (check_trust || require_sandbox).then(|| Self {
            home: home.into(),
            workspace: workspace.into(),
            user: file
                .user_source
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(home).join(".config/otto/config.toml")),
            check_trust,
            expected_digest: file.executable_digest.clone(),
            require_sandbox,
        })
    }
    pub fn valid(&self) -> bool {
        let policy = crate::config::layered::policy_at(crate::config::layered::managed_path());
        policy.is_ok_and(|p| {
            !self.check_trust || p.as_ref().and_then(|p| p.allow_project_executables) != Some(false)
        }) && (!self.check_trust
            || crate::config::layered::authorized_digest(&self.home, &self.user, &self.workspace)
                .is_ok_and(|identity| identity.is_some() && identity == self.expected_digest))
    }
    fn arguments(&self, name: &str, arguments: &RawValue) -> Result<(), ToolResult> {
        if !self.valid() {
            return Err(revoked());
        }
        if name == "bash" && self.require_sandbox {
            let value: serde_json::Value = serde_json::from_str(arguments.get())
                .map_err(|_| ToolResult::error("invalid tool arguments").not_started())?;
            if value
                .get("sandbox_permissions")
                .and_then(serde_json::Value::as_str)
                == Some("require_escalated")
            {
                return Err(ToolResult::error(
                    "Managed policy prohibits execution outside the sandbox.",
                )
                .not_started());
            }
        }
        Ok(())
    }
    pub fn tools(
        &self,
        tools: Vec<Box<dyn Tool + Send + Sync>>,
    ) -> Vec<Box<dyn Tool + Send + Sync>> {
        tools
            .into_iter()
            .map(|inner| {
                Box::new(Guarded {
                    guard: self.clone(),
                    inner,
                }) as Box<dyn Tool + Send + Sync>
            })
            .collect()
    }
    pub fn watch_mcp(&self, servers: &Arc<crate::mcp::Servers>) {
        let weak = Arc::downgrade(servers);
        let guard = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let Some(servers) = weak.upgrade() else {
                    break;
                };
                if !guard.valid() {
                    servers.close().await;
                    break;
                }
            }
        });
    }
}

struct Guarded {
    guard: Guard,
    inner: Box<dyn Tool + Send + Sync>,
}

fn revoked() -> ToolResult {
    ToolResult::error("Project executable trust changed or was revoked; restart after reviewing and granting trust again.").not_started()
}

#[async_trait::async_trait]
impl Tool for Guarded {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
    fn yields_turn(&self) -> bool {
        self.inner.yields_turn()
    }
    fn batch_control(&self) -> otto_core::approval::BatchControl {
        self.inner.batch_control()
    }
    fn approval_spec(&self, arguments: &RawValue) -> Result<Option<ApprovalSpec>, ToolResult> {
        self.guard
            .arguments(&self.inner.definition().name, arguments)?;
        self.inner.approval_spec(arguments)
    }
    async fn approval_spec_async(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> Result<Option<ApprovalSpec>, ToolResult> {
        self.guard
            .arguments(&self.inner.definition().name, arguments)?;
        self.inner.approval_spec_async(arguments, cancel).await
    }
    async fn apply_approval(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
        request: &crate::approval::Request,
    ) -> Result<(), String> {
        self.guard
            .arguments(&self.inner.definition().name, arguments)
            .map_err(|r| r.content)?;
        self.inner.apply_approval(arguments, cancel, request).await
    }
    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if let Err(result) = self
            .guard
            .arguments(&self.inner.definition().name, arguments)
        {
            return result;
        }
        let child = cancel.child_token();
        let call = self.inner.execute(arguments, &child);
        tokio::pin!(call);
        loop {
            tokio::select! { result = &mut call => return result, _ = tokio::time::sleep(Duration::from_millis(500)) => { if !self.guard.valid() { child.cancel(); return call.await; } } }
        }
    }
    async fn execute_call(&self, request: ToolCall<'_>, cancel: &CancellationToken) -> ToolResult {
        if let Err(result) = self.guard.arguments(request.name, request.arguments) {
            return result;
        }
        let child = cancel.child_token();
        let call = self.inner.execute_call(request, &child);
        tokio::pin!(call);
        loop {
            tokio::select! { result = &mut call => return result, _ = tokio::time::sleep(Duration::from_millis(500)) => { if !self.guard.valid() { child.cancel(); return call.await; } } }
        }
    }
    async fn execute_approved(
        &self,
        request: ToolCall<'_>,
        cancel: &CancellationToken,
        approval: &crate::approval::Request,
    ) -> ToolResult {
        if let Err(result) = self.guard.arguments(request.name, request.arguments) {
            return result;
        }
        let child = cancel.child_token();
        let call = self.inner.execute_approved(request, &child, approval);
        tokio::pin!(call);
        loop {
            tokio::select! { result = &mut call => return result, _ = tokio::time::sleep(Duration::from_millis(500)) => { if !self.guard.valid() { child.cancel(); return call.await; } } }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Echo;
    #[async_trait::async_trait]
    impl Tool for Echo {
        fn definition(&self) -> ToolDefinition {
            super::super::definition("echo", "test", serde_json::json!({"type":"object"}))
        }
        async fn execute(&self, _: &RawValue, _: &CancellationToken) -> ToolResult {
            ToolResult {
                content: "ran".into(),
                ..Default::default()
            }
        }
    }
    #[tokio::test]
    async fn an_existing_tool_stops_after_revocation() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        crate::config::layered::set_trust(home.path(), workspace.path(), true).unwrap();
        let tool = Guarded {
            guard: Guard {
                home: home.path().into(),
                workspace: workspace.path().into(),
                user: home.path().join(".config/otto/config.toml"),
                check_trust: true,
                expected_digest: crate::config::layered::authorized_digest(
                    home.path(),
                    &home.path().join(".config/otto/config.toml"),
                    workspace.path(),
                )
                .unwrap(),
                require_sandbox: false,
            },
            inner: Box::new(Echo),
        };
        let arguments = RawValue::from_string("{}".into()).unwrap();
        assert_eq!(
            tool.execute(&arguments, &CancellationToken::new())
                .await
                .content,
            "ran"
        );
        crate::config::layered::set_trust(home.path(), workspace.path(), false).unwrap();
        assert!(
            tool.execute(&arguments, &CancellationToken::new())
                .await
                .is_error
        );
    }
    #[test]
    fn managed_sandbox_rejects_escalation_before_request_or_execution() {
        let guard = Guard {
            home: "/unused".into(),
            workspace: "/unused".into(),
            user: "/unused".into(),
            check_trust: false,
            expected_digest: None,
            require_sandbox: true,
        };
        let arguments = RawValue::from_string(
            r#"{"sandbox_permissions":"require_escalated","command":"echo outside"}"#.into(),
        )
        .unwrap();
        assert!(guard.arguments("bash", &arguments).is_err());
        let arguments = RawValue::from_string(
            r#"{"sandbox_permissions":"use_default","command":"echo confined"}"#.into(),
        )
        .unwrap();
        assert!(guard.arguments("bash", &arguments).is_ok());
    }
    #[tokio::test]
    async fn running_tools_are_cancelled_and_new_grants_do_not_revive_stale_snapshots() {
        struct UntilCancelled;
        #[async_trait::async_trait]
        impl Tool for UntilCancelled {
            fn definition(&self) -> ToolDefinition {
                super::super::definition("waiting", "test", serde_json::json!({"type":"object"}))
            }
            async fn execute(&self, _: &RawValue, cancel: &CancellationToken) -> ToolResult {
                cancel.cancelled().await;
                ToolResult::error("cancelled")
            }
        }
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join(".otto/skills/a/SKILL.md");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "first").unwrap();
        crate::config::layered::set_trust(home.path(), project.path(), true).unwrap();
        let file = crate::config::layered::load(
            home.path(),
            &home.path().join(".config/otto/config.toml"),
            false,
            project.path(),
        )
        .unwrap()
        .file;
        let guard = Guard::for_config(
            &file,
            home.path().to_str().unwrap(),
            project.path().to_str().unwrap(),
        )
        .unwrap();
        let tool = Guarded {
            guard: guard.clone(),
            inner: Box::new(UntilCancelled),
        };
        let arguments = RawValue::from_string("{}".into()).unwrap();
        let cancel = CancellationToken::new();
        let call = tool.execute(&arguments, &cancel);
        let change = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::write(&source, "second").unwrap();
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(call, change) })
                .await
                .unwrap();
        assert!(result.is_error);
        assert_eq!(result.content, "cancelled");
        crate::config::layered::set_trust(home.path(), project.path(), true).unwrap();
        assert!(
            !guard.valid(),
            "a changed source requires a new runtime snapshot"
        );
    }
}
