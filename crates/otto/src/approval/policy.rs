//! Native authority routing. A review cannot replace an explicit human gate.
use super::*;

pub(super) fn human_gate(request: &Request) -> bool {
    request.kind == Kind::Confirmation
        || request.authority == Authority::Unclassified
        || request.authority == Authority::Consent
        || matches!(
            request.subject.source.as_str(),
            "workflow" | "sandbox_read" | "sandbox_read_path" | "approval"
        )
        || (request.authority == Authority::Durable
            && !matches!(
                request.subject.source.as_str(),
                "memory"
                    | "skill"
                    | "reflection_skill"
                    | "config"
                    | "profile"
                    | "schedule"
                    | "delivery"
            ))
}

pub(super) fn rule(
    request: &Request,
    config: &otto_core::config::ApprovalConfig,
) -> Option<RuleDecision> {
    let body = serde_json::from_str::<serde_json::Value>(&request.subject.details).ok();
    // Built-in producers have fixed capability identities; MCP and plugin
    // producers journal the exact registered tool. Other native domains use
    // source selectors because their domain name is not a model tool name.
    let tool = match request.subject.source.as_str() {
        "sandbox" => "bash",
        "llm" | "conversation" => "request_approval",
        "mcp" | "plugin" => body
            .as_ref()
            .and_then(|v| v.get("tool"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        _ => "",
    };
    let command = body
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str());
    let analysis = command.map(crate::tool::shell::analyze);
    evaluate_rules(
        &config.rules,
        &request.subject.source,
        tool,
        analysis.as_ref(),
    )
}

/// Risk alone never supplies authorization. In particular, persistence needs
/// content/scope evidence and high-risk operations require exact authorization.
pub(super) fn constrained(mut assessment: ReviewAssessment, request: &Request) -> ReviewAssessment {
    if assessment.risk == RiskLevel::Critical {
        assessment.outcome = ReviewOutcome::Deny;
    } else if assessment.outcome == ReviewOutcome::Allow
        && (matches!(
            assessment.user_authorization,
            UserAuthorization::Unknown | UserAuthorization::Low
        ) || (assessment.risk == RiskLevel::High
            && assessment.user_authorization != UserAuthorization::High)
            || ((request.persistent || request.authority == Authority::Durable)
                && assessment.user_authorization != UserAuthorization::High))
    {
        assessment.outcome = ReviewOutcome::Ask;
        assessment.reason =
            "The requested content or operation lacks sufficient user authorization.".into();
    }
    assessment
}
