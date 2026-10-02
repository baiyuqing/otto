//! Reflection over HTTP: `POST /v1/sessions/{id}/reflect`, the generated
//! skills (`GET .../reflection/skills`, `POST .../reflection/skills/{name}/revert`),
//! and the notices background reflection queues (`GET .../notices`).
//!
//! `reflect` is held to the same window as a compaction: refused with 409
//! `turn_active` while a turn or a compaction runs on the session, and turns
//! are refused while it runs ([`super::Server::start_turn`] checks the same
//! slot). Listing and reverting skills touch the user's `~/.otto/skills`, not
//! the session, so they are not tied to the session's state; the `{id}` only
//! selects the controller and its configuration. They are as exposed as
//! `POST .../skills/{name}/enable`: any holder of the server token or socket
//! may call them, and a revert refuses a skill the user has edited.
//!
//! Notices are read by id and never removed, so every client of a session sees
//! each one.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, bad_request, error_response, json_response, not_found, turn_active};
use crate::app;
use crate::reflection::skillwrite::{RevertError, Reverted, Written, still_owned};
use crate::reflection::{Error, Report};

#[derive(Debug, Default, Deserialize)]
struct ReflectBody {
    #[serde(default)]
    focus: String,
}

#[derive(Debug, Serialize)]
struct SkillChange {
    name: String,
    /// `created` or `revised`.
    action: &'static str,
}

#[derive(Debug, Serialize)]
struct ReflectionWire {
    /// `ok`, or `noop` when there was nothing to reflect on or the run was
    /// skipped (`note` says why).
    status: &'static str,
    run_id: String,
    /// The one-line summary the terminal frontends print.
    line: String,
    /// Memory candidates queued for review.
    candidates: Vec<String>,
    skills: Vec<SkillChange>,
    /// Proposals dropped, by reason.
    dropped: BTreeMap<String, usize>,
    entries: usize,
    tainted: bool,
    skills_withheld: bool,
    truncated: bool,
    note: String,
}

impl From<&Report> for ReflectionWire {
    fn from(report: &Report) -> Self {
        Self {
            status: report.status.as_str(),
            run_id: report.run_id.clone(),
            line: report.line(),
            candidates: report.candidates.clone(),
            skills: report
                .skills
                .iter()
                .map(|(name, written)| SkillChange {
                    name: name.clone(),
                    action: match written {
                        Written::Created => "created",
                        Written::Revised => "revised",
                    },
                })
                .collect(),
            dropped: report
                .dropped
                .iter()
                .map(|(reason, count)| ((*reason).to_owned(), *count))
                .collect(),
            entries: report.entries,
            tainted: report.tainted,
            skills_withheld: report.skills_withheld,
            truncated: report.truncated,
            note: report.note.clone(),
        }
    }
}

/// `POST /v1/sessions/{id}/reflect`.
pub async fn reflect(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let parsed: ReflectBody = if body.iter().all(u8::is_ascii_whitespace) {
        ReflectBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return bad_request("invalid JSON body"),
        }
    };

    let cancel = {
        let mut state = session.lock();
        if state.busy() {
            return turn_active("a turn is already active for this session");
        }
        let cancel = server.cancel_token().child_token();
        state.compacting = Some(cancel.clone());
        cancel
    };

    // Only shutdown or closing the session cancels; axum gives a handler no
    // client-disconnect signal (see `compact`).
    let result = session.ctrl.reflect(&parsed.focus, &cancel).await;
    cancel.cancel();
    session.lock().compacting = None;
    server.start_next(&session);
    server.bump_status();

    match result {
        Ok(report) => {
            server.logger().info(
                "reflection_finished",
                &[
                    ("session_id", id),
                    ("candidates", report.candidates.len().to_string()),
                    ("skills", report.skills.len().to_string()),
                ],
            );
            json_response(StatusCode::OK, &ReflectionWire::from(&report))
        }
        Err(Error::Read(message)) if message == app::PROMPT_ACTIVE => {
            turn_active("a turn is already active for this session")
        }
        // Server shutting down or the session closed; nothing to write.
        Err(Error::Cancelled) => Response::new(axum::body::Body::empty()),
        Err(
            error @ (Error::Disabled
            | Error::NoSession
            | Error::MemoryUnavailable
            | Error::BoundaryClosed),
        ) => error_response(
            StatusCode::CONFLICT,
            "reflection_unavailable",
            &error.to_string(),
        ),
        Err(error) => {
            let message = error.to_string();
            server.logger().error(
                "reflection_error",
                &[("session_id", id), ("error", message.clone())],
            );
            error_response(StatusCode::CONFLICT, "reflection_failed", &message)
        }
    }
}

#[derive(Debug, Serialize)]
struct GeneratedSkillWire {
    name: String,
    run_id: String,
    session_id: String,
    reason: String,
    created_at: String,
    updated_at: String,
    /// Whether the file still has the content reflection last wrote; false
    /// once it has been edited or removed by hand, when `revert` refuses it.
    owned: bool,
}

#[derive(Debug, Serialize)]
struct GeneratedSkillList {
    /// False when reflection is turned off, in which case `skills` is empty.
    enabled: bool,
    skills: Vec<GeneratedSkillWire>,
}

/// `GET /v1/sessions/{id}/reflection/skills`.
pub async fn list_skills(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let reflector = &session.ctrl.builder().reflector;
    let rows = match reflector.generated_skills() {
        Ok(rows) => rows,
        Err(Error::Disabled) => {
            return json_response(
                StatusCode::OK,
                &GeneratedSkillList {
                    enabled: false,
                    skills: Vec::new(),
                },
            );
        }
        Err(error) => {
            server.logger().error(
                "reflection_error",
                &[("session_id", id), ("error", error.to_string())],
            );
            return error_response(
                StatusCode::CONFLICT,
                "reflection_failed",
                &error.to_string(),
            );
        }
    };
    let roots = session.ctrl.reflection_skill_roots();
    let skills = rows
        .into_iter()
        .map(|row| GeneratedSkillWire {
            owned: roots.as_ref().is_some_and(|roots| still_owned(roots, &row)),
            name: row.name,
            run_id: row.run_id,
            session_id: row.session_id,
            reason: row.reason,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
        .collect();
    json_response(
        StatusCode::OK,
        &GeneratedSkillList {
            enabled: true,
            skills,
        },
    )
}

#[derive(Debug, Serialize)]
struct RevertWire {
    name: String,
    /// `restored` (the previous version is back) or `removed` (reflection
    /// created the skill and it is gone).
    result: &'static str,
}

/// `POST /v1/sessions/{id}/reflection/skills/{name}/revert`.
pub async fn revert_skill(
    State(server): State<Arc<Server>>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let Some(roots) = session.ctrl.reflection_skill_roots() else {
        return error_response(
            StatusCode::CONFLICT,
            "skills_unavailable",
            "skills are not available (check [skills].enabled)",
        );
    };
    match session.ctrl.builder().reflector.revert_skill(&roots, &name) {
        Ok(reverted) => {
            server.logger().info(
                "reflection_skill_reverted",
                &[("session_id", id), ("skill", name.clone())],
            );
            json_response(
                StatusCode::OK,
                &RevertWire {
                    name,
                    result: match reverted {
                        Reverted::Restored => "restored",
                        Reverted::Removed => "removed",
                    },
                },
            )
        }
        Err(error @ RevertError::InvalidName(_)) => bad_request(&error.to_string()),
        Err(error @ RevertError::NotGenerated(_)) => not_found(&error.to_string()),
        Err(error @ RevertError::Edited { .. }) => {
            error_response(StatusCode::CONFLICT, "skill_not_owned", &error.to_string())
        }
        Err(error @ RevertError::Failed(_)) => {
            server.logger().error(
                "reflection_error",
                &[("session_id", id), ("error", error.to_string())],
            );
            error_response(StatusCode::CONFLICT, "revert_failed", &error.to_string())
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct NoticesQuery {
    /// Return only notices with an id above this; 0 or absent for all kept.
    #[serde(default)]
    after: u64,
}

#[derive(Debug, Serialize)]
struct NoticeWire {
    id: u64,
    text: String,
}

#[derive(Debug, Serialize)]
struct NoticeList {
    notices: Vec<NoticeWire>,
    /// The id of the newest notice ever queued for this session; pass it as
    /// `after` next time.
    last: u64,
}

/// `GET /v1/sessions/{id}/notices`.
pub async fn notices(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Query(query): Query<NoticesQuery>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let (lines, last) = session.ctrl.notices_since(query.after);
    json_response(
        StatusCode::OK,
        &NoticeList {
            notices: lines
                .into_iter()
                .map(|(id, text)| NoticeWire { id, text })
                .collect(),
            last,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reflection::Status;

    #[test]
    fn a_report_serializes_with_its_line_skills_and_dropped_reasons() {
        let report = Report {
            run_id: "run-1".into(),
            status: Status::Ok,
            candidates: vec!["cand-1".into()],
            dropped: BTreeMap::from([("evidence_quote_mismatch", 2)]),
            skills: vec![
                ("lint-gate".into(), Written::Created),
                ("fmt-check".into(), Written::Revised),
            ],
            skills_withheld: false,
            entries: 12,
            tainted: true,
            truncated: false,
            note: String::new(),
        };
        let value = serde_json::to_value(ReflectionWire::from(&report)).expect("json");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["candidates"], serde_json::json!(["cand-1"]));
        assert_eq!(
            value["skills"],
            serde_json::json!([
                {"name": "lint-gate", "action": "created"},
                {"name": "fmt-check", "action": "revised"},
            ])
        );
        assert_eq!(
            value["dropped"],
            serde_json::json!({"evidence_quote_mismatch": 2})
        );
        assert_eq!(value["tainted"], true);
        let line = value["line"].as_str().expect("line");
        assert!(
            line.contains("1 candidate(s) queued") && line.contains("created skill lint-gate"),
            "{line}"
        );
    }

    #[test]
    fn a_skipped_run_serializes_as_noop_with_its_note() {
        let report = Report::skipped_for_test();
        let value = serde_json::to_value(ReflectionWire::from(&report)).expect("json");
        assert_eq!(value["status"], "noop");
        assert_eq!(value["note"], "test");
        assert_eq!(value["line"], "reflection: skipped (test)");
    }
}
