//! Session-scoped skill catalog and enablement routes.
//!
//! The runner owns the active catalog, while the per-skill state is persisted
//! in the process configuration. A mutation affects runners built after Otto
//! restarts; it never mutates an in-flight runner.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use super::{Server, json_response, not_found};

#[derive(Debug, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub location: String,
}

impl From<&crate::skill::Skill> for Skill {
    fn from(skill: &crate::skill::Skill) -> Self {
        Self {
            name: skill.name.clone(),
            description: skill.description.clone(),
            location: skill.directory.display().to_string(),
        }
    }
}

#[derive(Serialize)]
struct SkillList {
    skills: Vec<Skill>,
}

#[derive(Serialize)]
struct SkillMutation {
    name: String,
    enabled: bool,
    restart_required: bool,
}

pub async fn list(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let skills = session
        .ctrl
        .skills()
        .skills()
        .iter()
        .map(Skill::from)
        .collect();
    json_response(StatusCode::OK, &SkillList { skills })
}

pub async fn get(
    State(server): State<Arc<Server>>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    let catalog = session.ctrl.skills();
    let Some(skill) = catalog.lookup(&name) else {
        return not_found("skill not found");
    };
    json_response(StatusCode::OK, &Skill::from(skill))
}

pub async fn enable(
    State(server): State<Arc<Server>>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    set_enabled(server, id, name, true).await
}

pub async fn disable(
    State(server): State<Arc<Server>>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    set_enabled(server, id, name, false).await
}

async fn set_enabled(server: Arc<Server>, id: String, name: String, enabled: bool) -> Response {
    let Some(session) = server.lookup(&id) else {
        return not_found("session not found");
    };
    match crate::config::set_skill_disabled_file(
        &session.ctrl.builder().config_path,
        &name,
        !enabled,
    ) {
        Ok(()) => json_response(
            StatusCode::OK,
            &SkillMutation {
                name,
                enabled,
                restart_required: true,
            },
        ),
        Err(error) => super::error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "skill_config_write_failed",
            &error.to_string(),
        ),
    }
}
