//! The registry of API operations: one [`OperationId`] per method handler of
//! [`crate::Server::router`], with its HTTP method and route pattern.
//!
//! The list below is the single source of the route table. The router is built
//! from it through [`super::OperationRoutes`], and the authorization middleware
//! looks requests up in it, so a handler cannot be served without an id.

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::http::Method;

macro_rules! operations {
    ($($variant:ident => $method:ident $path:literal,)+) => {
        /// One API operation: one method of one route.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum OperationId {
            $($variant,)+
        }

        impl OperationId {
            /// Every operation, in registration order.
            pub const ALL: &[OperationId] = &[$(OperationId::$variant,)+];

            /// The HTTP method of this operation.
            pub fn method(self) -> Method {
                match self {
                    $(OperationId::$variant => Method::$method,)+
                }
            }

            /// The axum route pattern of this operation.
            pub fn path(self) -> &'static str {
                match self {
                    $(OperationId::$variant => $path,)+
                }
            }

            /// The variant name, for logs.
            pub fn name(self) -> &'static str {
                match self {
                    $(OperationId::$variant => stringify!($variant),)+
                }
            }
        }
    };
}

operations! {
    ApprovalList => GET "/v1/approvals",
    ApprovalChanges => GET "/v1/approvals/changes",
    ApprovalEvents => GET "/v1/approvals/events",
    ApprovalGet => GET "/v1/approvals/{id}",
    ApprovalDecide => POST "/v1/approvals/{id}/decision",
    ApprovalWithdraw => POST "/v1/approvals/{id}/withdraw",
    ConfigProviderList => GET "/v1/config/providers",
    ConfigProviderGet => GET "/v1/config/providers/{name}",
    ConfigModelList => GET "/v1/config/models",
    ConfigChangePreview => POST "/v1/config/changes",
    ConfigChangeConfirm => POST "/v1/config/changes/{id}",
    ConfigChangeCancel => DELETE "/v1/config/changes/{id}",
    SessionCreate => POST "/v1/sessions",
    SessionList => GET "/v1/sessions",
    SessionGet => GET "/v1/sessions/{id}",
    SessionRename => PATCH "/v1/sessions/{id}",
    SessionDelete => DELETE "/v1/sessions/{id}",
    SessionDeletePermanent => DELETE "/v1/sessions/{id}/permanent",
    SessionArchive => POST "/v1/sessions/{id}/archive",
    SessionFork => POST "/v1/sessions/{id}/fork",
    SessionModelSet => POST "/v1/sessions/{id}/model",
    SessionThinkingSet => POST "/v1/sessions/{id}/thinking",
    SessionForkPoints => GET "/v1/sessions/{id}/fork-points",
    SessionHistory => GET "/v1/sessions/{id}/history",
    SessionLocalCommand => POST "/v1/sessions/{id}/local-commands",
    SessionContext => GET "/v1/sessions/{id}/context",
    SessionTrace => GET "/v1/sessions/{id}/trace",
    SessionTraceSpan => GET "/v1/sessions/{id}/trace/{span_id}",
    SessionApprovalMessage => POST "/v1/sessions/{id}/approvals/message",
    SessionInputAdmit => POST "/v1/sessions/{id}/inputs",
    SessionSteerList => GET "/v1/sessions/{id}/tasks/steer",
    SessionSteer => POST "/v1/sessions/{id}/tasks/steer",
    SessionSteerInterrupt => POST "/v1/sessions/{id}/tasks/steer/interrupt",
    SessionSteerWithdraw => DELETE "/v1/sessions/{id}/tasks/steer/{message_id}",
    SessionTaskControl => POST "/v1/sessions/{id}/tasks/control",
    SessionApprovalDecide => POST "/v1/sessions/{id}/approvals/{approval_id}",
    SessionTurnStart => POST "/v1/sessions/{id}/turns",
    SessionTurnGet => GET "/v1/sessions/{id}/turns/{turn_id}",
    SessionTurnEvents => GET "/v1/sessions/{id}/turns/{turn_id}/events",
    SessionTurnCancel => POST "/v1/sessions/{id}/turns/{turn_id}/cancel",
    SessionCompact => POST "/v1/sessions/{id}/compact",
    SessionWakeReplies => GET "/v1/sessions/{id}/wake-replies",
    SessionTaskList => GET "/v1/sessions/{id}/tasks",
    SessionTaskObserver => GET "/v1/sessions/{id}/tasks/observer",
    SessionDeliveryGet => GET "/v1/sessions/{id}/deliveries/{approval_id}",
    SessionTaskGet => GET "/v1/sessions/{id}/tasks/{task_id}",
    SessionTaskCancel => POST "/v1/sessions/{id}/tasks/{task_id}/cancel",
    SessionTimerList => GET "/v1/sessions/{id}/timers",
    SessionTimerCancel => POST "/v1/sessions/{id}/timers/{timer_id}/cancel",
    SessionMcpList => GET "/v1/sessions/{id}/mcp",
    MemoryRecordList => GET "/v1/sessions/{id}/memory/records",
    MemoryRecordCreate => POST "/v1/sessions/{id}/memory/records",
    MemoryRecordGet => GET "/v1/sessions/{id}/memory/records/{record_id}",
    MemoryRecordForget => DELETE "/v1/sessions/{id}/memory/records/{record_id}",
    MemoryRecordStateSet => POST "/v1/sessions/{id}/memory/records/{record_id}/state",
    MemoryRecordVerify => POST "/v1/sessions/{id}/memory/records/{record_id}/verify",
    MemoryRecordHistory => GET "/v1/sessions/{id}/memory/records/{record_id}/history",
    MemoryRecordDiff => GET "/v1/sessions/{id}/memory/records/{record_id}/diff",
    MemoryRecordRestore => POST "/v1/sessions/{id}/memory/records/{record_id}/restore",
    MemoryRecordPropose => POST "/v1/sessions/{id}/memory/records/{record_id}/propose",
    MemorySearch => GET "/v1/sessions/{id}/memory/search",
    MemoryCandidateList => GET "/v1/sessions/{id}/memory/candidates",
    MemoryCandidateReview => POST "/v1/sessions/{id}/memory/candidates/{candidate_id}/review",
    SkillManagerList => GET "/v1/sessions/{id}/skill-manager",
    SkillManagerInfo => GET "/v1/sessions/{id}/skill-manager/{name}",
    SkillList => GET "/v1/sessions/{id}/skills",
    SkillGet => GET "/v1/sessions/{id}/skills/{name}",
    SkillEnable => POST "/v1/sessions/{id}/skills/{name}/enable",
    SkillDisable => POST "/v1/sessions/{id}/skills/{name}/disable",
    SandboxReload => POST "/v1/sandbox/reload",
    SessionSandboxChangeApply => POST "/v1/sessions/{id}/sandbox/changes",
    SessionApprovalEvents => GET "/v1/sessions/{id}/approval-events",
    SessionApprovalModeGet => GET "/v1/sessions/{id}/approval-mode",
    SessionApprovalModeSet => PUT "/v1/sessions/{id}/approval-mode",
    SessionApprovalPolicyGet => GET "/v1/sessions/{id}/approval-policy",
    SessionApprovalPolicySet => PUT "/v1/sessions/{id}/approval-policy",
    ScheduleClaim => POST "/v1/schedule/claim",
    ScheduleDelivered => POST "/v1/schedule/{id}/delivered",
    TaskList => GET "/v1/tasks",
    TaskGet => GET "/v1/tasks/{parent_session}/{task_id}",
    WorkflowList => GET "/v1/workflows",
    WorkflowStart => POST "/v1/workflows",
    WorkflowGet => GET "/v1/workflows/{id}",
    WorkflowEvents => GET "/v1/workflows/{id}/events",
    WorkflowResume => POST "/v1/workflows/{id}/resume",
    WorkflowFork => POST "/v1/workflows/{id}/fork",
    WorkflowCancel => POST "/v1/workflows/{id}/cancel",
    WorkflowRequestApprove => POST "/v1/workflows/requests/{id}/approve",
    WorkflowRequestReject => POST "/v1/workflows/requests/{id}/reject",
    WorkspaceList => GET "/v1/workspaces",
    WorkspaceRegister => POST "/v1/workspaces",
    WorkspaceRemove => DELETE "/v1/workspaces",
    WorkspaceDiff => GET "/v1/workspaces/diff",
    FsDirList => GET "/v1/fs/dirs",
    Info => GET "/v1/info",
    Status => GET "/v1/status",
    Usage => GET "/v1/usage",
    UsageDaily => GET "/v1/usage/daily",
    OpenApi => GET "/v1/openapi.yaml",
    Healthz => GET "/healthz",
    Metrics => GET "/metrics",
}

impl OperationId {
    /// False for the two routes the token gate serves without a token on a
    /// loopback listener: they carry no principal and need no authorization.
    pub fn requires_principal(self) -> bool {
        !matches!(self, OperationId::Healthz | OperationId::Metrics)
    }

    /// The operations registered for `path`, a route pattern as axum's
    /// `MatchedPath` reports it. Empty when no operation uses that pattern.
    pub fn for_path(path: &str) -> &'static [OperationId] {
        static BY_PATH: OnceLock<HashMap<&'static str, Vec<OperationId>>> = OnceLock::new();
        BY_PATH
            .get_or_init(|| {
                let mut by_path: HashMap<&'static str, Vec<OperationId>> = HashMap::new();
                for operation in OperationId::ALL {
                    by_path
                        .entry(operation.path())
                        .or_default()
                        .push(*operation);
                }
                by_path
            })
            .get(path)
            .map_or(&[], Vec::as_slice)
    }
}
