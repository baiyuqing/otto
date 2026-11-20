//! Plain-text rendering of management replies.
//!
//! The terminal frontends and the HTTP server print the same text for the same
//! [`Reply`](super::Reply), so the formatting lives here and not in a frontend.

use std::fmt::Write as _;

use chrono::Utc;

use crate::mcp::{Era, ServerState};
use crate::memory::{ReviewDecision, SearchResult};
use crate::schedule::remind::{NO_TIMERS, timer_line};
use crate::skill;
use crate::subagent::format::{first_runes, one_line};

/// Content-only rendering shared by the REPL and TUI management presenters.
pub fn format_management_reply(reply: &crate::app::management::Reply) -> String {
    use crate::app::management::Reply;
    match reply {
        Reply::Tasks(rows) => {
            if rows.is_empty() {
                "No tasks.".into()
            } else {
                rows.iter()
                    .map(|t| format!("{} [{}] {}", t.id, t.status.as_str(), t.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Reply::Task(t) => format!(
            "{} [{}] {}\nResult: {}\nError: {}\nTranscript: {}",
            t.id,
            t.status.as_str(),
            t.description,
            t.result,
            t.error,
            t.session_path
        ),
        Reply::TaskCancelled(id) => format!("Cancelled task: {id}"),

        Reply::MemoryList(page) => render_list_result(&page.records, &page.next_cursor),
        Reply::MemoryRecord(record) => render_list_result(std::slice::from_ref(record), ""),
        Reply::MemorySearch(result) => render_search_result(result),
        Reply::MemoryCandidates(rows) => render_pending_candidates(rows),
        Reply::MemoryCandidatePage(page) => render_pending_candidates(&page.candidates),
        Reply::MemoryForgotten {
            tombstone,
            previous_revision,
            cleanup,
        } => {
            let mut text = format!("forgot {} (revision {})", tombstone.id, previous_revision);
            for entry in cleanup {
                text.push_str(&format!(
                    "\n  {}: {} ({})",
                    entry.status.as_str(),
                    entry.copy,
                    entry.detail
                ));
            }
            text
        }
        Reply::MemoryHistory(history) => crate::memory::history::render_history(history),
        Reply::MemoryDiff { from, to } => crate::memory::history::render_comparison(from, to),
        Reply::MemoryRestored(record) => format!(
            "restored {} as revision {} (from revision {})",
            record.id,
            record.revision,
            record
                .metadata
                .get(crate::memory::RESTORED_FROM_REVISION_METADATA)
                .map(String::as_str)
                .unwrap_or("?")
        ),
        Reply::MemoryStateChanged(record) => {
            let mut text = format!(
                "state of {} is now {} (revision {})",
                record.id,
                record.state.as_str(),
                record.revision
            );
            if !record.superseded_by.is_empty() {
                text.push_str(&format!(" superseded_by={}", record.superseded_by));
            }
            if !record.state_reason.is_empty() {
                text.push_str(&format!(" reason={}", record.state_reason));
            }
            text
        }
        Reply::MemoryProposed(outcome) => {
            use crate::memory::proposals::Outcome;
            match outcome {
                Outcome::NoChange { explanation } => format!("no change proposed: {explanation}"),
                Outcome::Proposed(candidate) => format!(
                    "proposed {} of {} as candidate {} (review with /memory review {} accept|reject): {}",
                    candidate.action.as_str(),
                    candidate.target_id,
                    candidate.id,
                    candidate.id,
                    candidate.reason
                ),
                Outcome::Rejected(reason) => format!("proposal rejected: {reason}"),
                Outcome::Unavailable(reason) => format!("proposal unavailable: {reason}"),
            }
        }
        Reply::MemoryVerified(record) => format!(
            "verified {} (revision {}) verified_at={} review_at={}",
            record.id,
            record.revision,
            record
                .verified_at
                .map(|time| time.to_string())
                .unwrap_or_default(),
            record
                .review_at
                .map(|time| time.to_string())
                .unwrap_or_else(|| "none".into())
        ),
        Reply::MemoryReviewed(decision, result) => {
            let id = &result.candidate.id;
            match (decision, &result.record, &result.tombstone) {
                (ReviewDecision::Reject, _, _) => format!("rejected {id}"),
                (_, Some(r), _) => {
                    format!("accepted {id} as record {} (revision {})", r.id, r.revision)
                }
                (_, None, Some(t)) => format!("accepted {id}: forgot {}", t.id),
                _ => format!("accepted {id}"),
            }
        }
        Reply::Remembered(record) => format!(
            "remembered {} (scope={}/{} kind={})",
            record.id, record.scope.namespace, record.scope.id, record.kind
        ),
        Reply::SkillManager(inventory) => {
            let rows = inventory
                .skills
                .iter()
                .map(|row| {
                    format!(
                        "{} [preference: {}] {}",
                        row.skill.name,
                        if row.enabled { "enabled" } else { "disabled" },
                        row.skill.description
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if inventory.global_enabled {
                rows
            } else {
                format!(
                    "Skills globally disabled. Set [skills].enabled = true in Otto config and restart Otto; restarting alone will not enable skills.\n{rows}"
                )
            }
        }
        Reply::SkillInfo(info) => format!(
            "Skill: {}\nDescription: {}\nLocation: {}\nSource: {}\n{}\n\n{}",
            info.skill.name,
            info.skill.description,
            info.skill.directory.display(),
            info.skill.path.display(),
            info.contract_check.as_deref().unwrap_or(""),
            info.body
        ),
        Reply::Skills(catalog) => {
            if catalog.is_empty() {
                "No skills found.".into()
            } else {
                let mut s = "Available skills:".to_string();
                for item in catalog.skills() {
                    s.push_str(&skill_list_line(item));
                }
                s
            }
        }
        Reply::MemoryServiceResult { text: message, .. }
        | Reply::SkillSet(message)
        | Reply::Notice(message)
        | Reply::Usage(message) => message.clone(),
        Reply::SkillDetail {
            skill: found,
            body,
            contract_check,
        } => {
            let contract = contract_check
                .as_ref()
                .map(|v| format!("\n{v}"))
                .unwrap_or_default();
            format!(
                "Skill: {}\nLocation: {}\nDescription: {}{}\n\n{}",
                found.name,
                found.directory.display(),
                found.description,
                contract,
                body.trim_end()
            )
        }
        Reply::McpStatus(rows) => format_mcp_report(rows),
        Reply::Timers(items) => {
            if items.is_empty() {
                NO_TIMERS.into()
            } else {
                let now = Utc::now();
                items
                    .iter()
                    .map(|i| timer_line(i, now))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        Reply::TimerCancelled(item) => format!("canceled {}: {}", item.id, item.message),
    }
}

fn render_search_result(result: &SearchResult) -> String {
    if result.records.is_empty() {
        return "no matching records".to_string();
    }
    let mut content = format!("{} records:\n", result.records.len());
    for record in &result.records {
        let _ = writeln!(
            content,
            "id={} scope={}/{} kind={} key={} revision={} text={}",
            record.id,
            record.scope.namespace,
            record.scope.id,
            record.kind,
            record.key,
            record.revision,
            record.text
        );
    }
    content.trim_end_matches('\n').to_string()
}

fn render_list_result(records: &[crate::memory::Record], next_cursor: &str) -> String {
    if records.is_empty() {
        return "no records".to_string();
    }
    let mut content = format!("{} records:\n", records.len());
    for record in records {
        let _ = writeln!(
            content,
            "id={} scope={}/{} kind={} key={} revision={} state={}{}{} created_at={} updated_at={} \
             verified_at={} review_at={} text={}",
            record.id,
            record.scope.namespace,
            record.scope.id,
            record.kind,
            record.key,
            record.revision,
            record.state.as_str(),
            if record.superseded_by.is_empty() {
                String::new()
            } else {
                format!(" superseded_by={}", record.superseded_by)
            },
            if record.state_reason.is_empty() {
                String::new()
            } else {
                format!(" state_reason={}", record.state_reason)
            },
            record.created_at,
            record.updated_at,
            record
                .verified_at
                .map(|time| time.to_string())
                .unwrap_or_default(),
            record
                .review_at
                .map(|time| time.to_string())
                .unwrap_or_default(),
            record.text
        );
        if let Some(applicability) = record.metadata.get(crate::memory::evidence::APPLICABILITY) {
            let anchors = crate::memory::evidence::read(&record.metadata)
                .and_then(|anchors| {
                    serde_json::to_string(&anchors)
                        .map_err(|_| crate::memory::invalid_request("invalid evidence"))
                })
                .unwrap_or_else(|_| "invalid evidence metadata".into());
            let _ = writeln!(
                content,
                "  applicability={applicability} evidence={anchors}"
            );
        }
    }
    if !next_cursor.is_empty() {
        let _ = writeln!(content, "next_cursor={next_cursor}");
    }
    content.trim_end_matches('\n').to_string()
}

/// Renders one compact catalog row. Bare `/skill` deliberately lists every
/// available name, but leaves potentially long descriptions and the
/// per-skill contract-check lookup to `/skill <name>` so the TUI does not
/// retain a large, expensive-to-render transcript entry.
fn skill_list_line(skill: &skill::Skill) -> String {
    let contract = if skill.contract.is_some() {
        " [contract]"
    } else {
        ""
    };
    format!("\n- {}{}", skill.name, contract)
}

fn render_pending_candidates(candidates: &[crate::memory::Candidate]) -> String {
    if candidates.is_empty() {
        return "no pending candidates".to_string();
    }
    let mut out = format!("{} pending candidates:", candidates.len());
    for candidate in candidates {
        let proposed = &candidate.proposed;
        let origin = proposed.source.origin.map_or("unknown", |o| o.as_str());
        let _ = write!(
            out,
            "\nid={} scope={}/{} action={} kind={} key={} confidence={} origin={} text={} reason={}",
            candidate.id,
            proposed.scope.namespace,
            proposed.scope.id,
            candidate.action.as_str(),
            proposed.kind,
            proposed.key,
            proposed.confidence,
            origin,
            first_runes(&one_line(&proposed.text), 120),
            first_runes(&one_line(&candidate.reason), 120),
        );
        if let Some(line) = crate::memory::advice::advice_line(proposed) {
            let _ = write!(out, "\n  {line}");
        }
    }
    out
}

/// The text of the MCP status rows in `Reply::McpStatus`. `pub` because the
/// REPL `/mcp` command in the frontend also calls it. A free function so the
/// per-state line format has a test that does not need a runner with a real
/// MCP server behind it.
pub fn format_mcp_report(servers: &[crate::mcp::ServerStatus]) -> String {
    if servers.is_empty() {
        return "No MCP servers configured.".to_string();
    }
    let mut out = "MCP servers:".to_string();
    for server in servers {
        let era = match &server.era {
            Some(Era::Modern) => "modern".to_string(),
            Some(Era::Legacy(version)) => format!("legacy {version}"),
            None => "-".to_string(),
        };
        let state = match &server.state {
            ServerState::Connected { tools } => format!("connected ({tools} tools)"),
            ServerState::Connecting => "connecting".to_string(),
            ServerState::Disabled => "disabled".to_string(),
            ServerState::NeedsLogin => "needs login".to_string(),
            ServerState::Failed(message) => format!("failed: {message}"),
        };
        let _ = write!(
            out,
            "\n- {}: {} ({}, {})",
            server.name, state, server.transport, era
        );
    }
    out
}

#[cfg(test)]
#[path = "reply_text_tests.rs"]
mod tests;
