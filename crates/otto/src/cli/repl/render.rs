//! Rendering agent events to the terminal.

use super::*;

/// Renders one event. Returns true when it wrote an agent error, so the caller
/// does not print the same failure twice.
pub(super) fn render_event(stdout: &mut dyn Write, stderr: &mut dyn Write, event: &Event) -> bool {
    match event {
        Event::ApprovalChanged { request } => {
            if request
                .decision
                .as_ref()
                .is_some_and(|d| d.transport == "auto-mode")
            {
                let _ = writeln!(
                    stdout,
                    "[AUTO] Approval {} automatically approved; application {:?}",
                    request.id, request.application
                );
            }
            let _ = writeln!(
                stdout,
                "\n[approval] {} ({}) {:?}\n{}",
                request.title, request.id, request.state, request.subject.details
            );
            if let Some(review) = &request.review {
                let _ = writeln!(
                    stdout,
                    "Review: {:?}{}",
                    review.status,
                    review
                        .assessment
                        .as_ref()
                        .map(|assessment| format!(" · {}", assessment.reason))
                        .unwrap_or_default()
                );
            }
            if request.is_human_pending() {
                let _ = writeln!(
                    stdout,
                    "Approve in Otto: /approve {}\nReject: /deny {} [feedback]\nWithdraw: /withdraw {}",
                    request.id, request.id, request.id
                );
            }
        }
        Event::TextDelta { text } => {
            let _ = write!(stdout, "{text}");
        }
        Event::ToolCallStarted {
            tool_name,
            tool_call_id,
            ..
        } => {
            let _ = writeln!(stdout, "\n[tool] {tool_name} ({tool_call_id})");
        }
        Event::ToolCallFinished {
            operation_id,
            tool_name,
            result,
            outcome,
            ..
        } => {
            let label = tool_outcome_label(outcome);
            if outcome.effect_certainty == EffectCertainty::Unknown {
                let _ = writeln!(
                    stdout,
                    "[tool warning] {label} (operation {operation_id}): {}",
                    first_line(&result.content)
                );
            } else {
                let _ = writeln!(
                    stdout,
                    "[tool result] {label} (operation {operation_id}): {}",
                    first_line(&result.content)
                );
            }
            // A bash approval request puts the /approve command, the command,
            // and the justification on their own lines after the first.
            if tool_name == "bash" && result.is_error {
                for line in result.content.lines().filter(|line| {
                    ["Approve in Otto: ", "Command: ", "Justification: "]
                        .iter()
                        .any(|prefix| line.starts_with(prefix))
                }) {
                    let _ = writeln!(stdout, "{line}");
                }
            }
        }
        Event::CompactionCompleted { compaction } => {
            let _ = write!(stdout, "{}", compaction_line(compaction));
        }
        Event::ProviderRetry {
            attempt,
            max_attempts,
            delay,
            reason,
            interrupted_output,
            ..
        } => {
            if *interrupted_output {
                let _ = writeln!(
                    stdout,
                    "\n[{}]",
                    otto_core::wire::transcript::INTERRUPTED_OUTPUT_NOTICE
                );
            }
            let _ = writeln!(
                stderr,
                "[provider retry] attempt {attempt}/{max_attempts} in {}ms: {}",
                delay.as_millis(),
                first_line(reason)
            );
        }
        Event::CompactionWarning { message } | Event::MemoryWarning { message } => {
            let _ = writeln!(stderr, "{message}");
        }
        Event::AgentError { message } => {
            let _ = writeln!(stderr, "{message}");
            return true;
        }
        Event::Notification { text, .. } => {
            let _ = writeln!(stdout, "\n{text}");
        }
        _ => {}
    }
    false
}

fn tool_outcome_label(outcome: &OperationOutcome) -> &'static str {
    match outcome.effect_certainty {
        EffectCertainty::Unknown => "Outcome unknown — check effects before retrying",
        EffectCertainty::NotStarted => "Did not run",
        EffectCertainty::KnownNoEffect => "Known no effect",
        EffectCertainty::Completed => match outcome.disposition {
            OperationDisposition::Succeeded => "Completed",
            OperationDisposition::Error => "Failed with a known result",
            _ => "Completed",
        },
    }
}
