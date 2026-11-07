//! Best-effort credential protection for new journal content only. Imported
//! history and protocol identities are preserved; live tool calls never borrow
//! these recording copies.

use otto_core::agent::redactor::Redactor;
use std::sync::LazyLock;

pub(super) static RECORD_REDACTOR: LazyLock<Redactor> = LazyLock::new(|| Redactor::new(&[]));

pub(super) fn custom_data(custom_type: &str, raw: &str) -> String {
    if matches!(
        custom_type,
        "otto.local_command"
            | "otto.task_spec"
            | "otto.task_result"
            | "otto.hook.arguments"
            | "otto.hook.result"
            | "otto.turn"
            | "otto.plan"
            | "otto.provider_recovery"
    ) {
        // Approval content retains its exemption, including hook audit copies.
        if matches!(custom_type, "otto.hook.arguments" | "otto.hook.result")
            && serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .and_then(|value| {
                    value
                        .get("tool")
                        .and_then(|tool| tool.as_str())
                        .map(str::to_owned)
                })
                .is_some_and(|tool| {
                    matches!(
                        tool.as_str(),
                        "request_approval" | "approval_pending" | "approval_revoke"
                    )
                })
        {
            return raw.to_owned();
        }
        RECORD_REDACTOR.redact_record_json(raw).get().to_owned()
    } else {
        raw.to_owned()
    }
}
