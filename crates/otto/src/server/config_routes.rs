//! Restricted profile configuration management routes.
//!
//! These handlers accept typed profile operations only. They never expose raw
//! TOML, environment values, or credentials; writes go through the native
//! configuration module's preview and CAS-backed commit path.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::config::{self as native, ProfileChange, ProfileField, ProfileProvider};
use crate::provider::openaicompat::Client as OpenAiClient;

use super::{Server, error_response, json_response};

const CHANGE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_MODEL_IDS: usize = 2_000;

/// `X-Request-Id`, or `X-Otto-Request-Id` as sent by older connect builds.
fn correlation(headers: &HeaderMap) -> String {
    let id = super::client_request_id(headers);
    if !id.is_empty() {
        return id;
    }
    headers
        .get("x-otto-request-id")
        .map(|value| super::request_id(value.as_bytes()))
        .unwrap_or_default()
}

#[cfg(test)]
mod correlation_tests {
    use super::*;

    #[test]
    fn x_request_id_wins_and_the_old_header_is_the_fallback() {
        let mut headers = HeaderMap::new();
        assert_eq!(correlation(&headers), "");
        headers.insert("x-otto-request-id", "old".parse().unwrap());
        assert_eq!(correlation(&headers), "old");
        headers.insert("x-request-id", "new".parse().unwrap());
        assert_eq!(correlation(&headers), "new");
    }
}

#[derive(Serialize)]
struct ProfileResponse {
    name: String,
    default: bool,
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    thinking: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    base_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    api_key_env: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    api_version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    workspace_id_env: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_window: Option<i64>,
}

fn profile(name: &str, file: &otto_core::config::File) -> Option<ProfileResponse> {
    let value = file.profiles.get(name)?;
    Some(ProfileResponse {
        name: name.to_string(),
        default: file.default_profile == name,
        provider: value.provider.clone(),
        model: value.model.clone(),
        thinking: value.thinking.clone(),
        base_url: value.base_url.clone(),
        api_key_env: value.api_key_env.clone(),
        api_version: value.api_version.clone(),
        workspace_id_env: value.workspace_id_env.clone(),
        context_window: value.context_window,
        compaction_window: value.compaction_window,
    })
}

fn load(server: &Server) -> Result<otto_core::config::File, Box<Response>> {
    native::load(&server.config_path).map_err(|error| {
        Box::new(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_unavailable",
            &error.to_string(),
        ))
    })
}

pub async fn profiles(State(server): State<std::sync::Arc<Server>>) -> Response {
    let file = match load(&server) {
        Ok(file) => file,
        Err(reply) => return *reply,
    };
    let mut names: Vec<_> = file.profiles.keys().collect();
    names.sort();
    let profiles: Vec<_> = names
        .into_iter()
        .filter_map(|name| profile(name, &file))
        .collect();
    json_response(StatusCode::OK, &serde_json::json!({"profiles": profiles}))
}

pub async fn get_profile(
    Path(name): Path<String>,
    State(server): State<std::sync::Arc<Server>>,
) -> Response {
    let file = match load(&server) {
        Ok(file) => file,
        Err(reply) => return *reply,
    };
    match profile(&name, &file) {
        Some(profile) => json_response(StatusCode::OK, &profile),
        None => error_response(
            StatusCode::NOT_FOUND,
            "profile_not_found",
            "profile not found",
        ),
    }
}

#[derive(Deserialize)]
pub struct ModelsQuery {
    profile: String,
}

pub async fn models(
    Query(query): Query<ModelsQuery>,
    State(server): State<std::sync::Arc<Server>>,
) -> Response {
    let file = match load(&server) {
        Ok(file) => file,
        Err(reply) => return *reply,
    };
    let Some(profile) = file.profiles.get(&query.profile) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "profile_not_found",
            "profile not found",
        );
    };
    if profile.provider == otto_core::config::PROVIDER_CHATGPT {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "models_unsupported",
            "the chatgpt provider cannot list models",
        );
    }
    if !matches!(
        profile.provider.as_str(),
        otto_core::config::PROVIDER_OPENAI_COMPATIBLE | otto_core::config::PROVIDER_CLAUDE
    ) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "profile has an unsupported provider",
        );
    }
    let environment: std::collections::HashMap<String, String> = std::env::vars().collect();
    let runtime = match otto_core::config::resolve(
        &file,
        &environment,
        &otto_core::config::SessionDefaults::default(),
        &otto_core::config::Overrides {
            profile: query.profile.clone(),
            ..Default::default()
        },
    ) {
        Ok(runtime) => runtime,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "api_key_unavailable",
                "profile connection or credentials are unavailable; check the profile and its environment bindings",
            );
        }
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = if runtime.provider == otto_core::config::PROVIDER_CLAUDE {
        crate::provider::claude::Client::new(
            &runtime.base_url,
            &runtime.api_key,
            &runtime.api_version,
            &runtime.workspace_id,
        )
        .list_models(&cancel)
        .await
    } else {
        OpenAiClient::new(&runtime.base_url, &runtime.api_key)
            .list_models(&cancel)
            .await
    };
    match result {
        Ok(mut ids) => {
            ids.truncate(MAX_MODEL_IDS);
            json_response(
                StatusCode::OK,
                &serde_json::json!({"profile": query.profile, "models": ids}),
            )
        }
        Err(error) => error_response(StatusCode::BAD_GATEWAY, "models_failed", &error.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ChangeRequest {
    SetDefaultProfile {
        profile: String,
    },
    CreateProfile {
        profile: String,
        provider: String,
        model: String,
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
    },
    SetProfileField {
        profile: String,
        field: String,
        value: String,
    },
    RemoveProfile {
        profile: String,
    },
}

fn provider(value: &str) -> Result<ProfileProvider, Box<Response>> {
    match value {
        otto_core::config::PROVIDER_OPENAI_COMPATIBLE => Ok(ProfileProvider::OpenAiCompatible),
        otto_core::config::PROVIDER_CHATGPT => Ok(ProfileProvider::ChatGpt),
        otto_core::config::PROVIDER_CLAUDE => Ok(ProfileProvider::Claude),
        _ => Err(Box::new(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "provider must be openai-compatible, claude or chatgpt",
        ))),
    }
}
fn field(value: &str) -> Result<ProfileField, Box<Response>> {
    match value {
        "model" => Ok(ProfileField::Model),
        "thinking" => Ok(ProfileField::Thinking),
        "base_url" => Ok(ProfileField::BaseUrl),
        "api_key_env" => Ok(ProfileField::ApiKeyEnv),
        "api_version" => Ok(ProfileField::ApiVersion),
        "workspace_id_env" => Ok(ProfileField::WorkspaceIdEnv),
        _ => Err(Box::new(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            "field is not editable",
        ))),
    }
}
fn change(request: ChangeRequest) -> Result<ProfileChange, Box<Response>> {
    Ok(match request {
        ChangeRequest::SetDefaultProfile { profile } => ProfileChange::SetDefault { profile },
        ChangeRequest::CreateProfile {
            profile,
            provider: provider_name,
            model,
            thinking,
            base_url,
            api_key_env,
        } => ProfileChange::Create {
            profile,
            provider: provider(&provider_name)?,
            model,
            thinking,
            base_url,
            api_key_env,
        },
        ChangeRequest::SetProfileField {
            profile,
            field: field_name,
            value,
        } => ProfileChange::SetField {
            profile,
            field: field(&field_name)?,
            value,
        },
        ChangeRequest::RemoveProfile { profile } => ProfileChange::Remove { profile },
    })
}

#[derive(Serialize)]
struct ChangeResponse {
    id: String,
    expires_in_seconds: u64,
    operation: String,
    profile: String,
    field: Option<String>,
    diff: String,
}

pub(super) async fn preview(
    State(server): State<std::sync::Arc<Server>>,
    headers: HeaderMap,
    Json(request): Json<ChangeRequest>,
) -> Response {
    let request_id = correlation(&headers);
    let change = match change(request) {
        Ok(change) => change,
        Err(reply) => return *reply,
    };
    let preview = match native::preview_profile_change(&server.config_path, &change) {
        Ok(preview) => preview,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_change",
                &error.to_string(),
            );
        }
    };
    let Some(service) = &server.approvals else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "approval_unavailable",
            "approval service unavailable",
        );
    };
    let artifact = crate::approval::profile_change::Artifact::prepare(change, &preview);
    let details = match serde_json::to_string(&artifact) {
        Ok(json) => json,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "approval_unavailable",
                "cannot prepare change",
            );
        }
    };
    let approval = match service.create(crate::approval::Proposal {
        kind: otto_core::approval::Kind::Authorization,
        authority: artifact.authority(),
        scope: otto_core::approval::Scope {
            workspace: server.info.workspace.clone(),
            session_id: String::new(),
            task_id: String::new(),
            run_id: String::new(),
        },
        subject: otto_core::approval::Subject {
            tool_call_id: String::new(),
            source: "config".into(),
            id: match super::new_id() {
                Ok(id) => id,
                Err(_) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "approval_unavailable",
                        "cannot create request identity",
                    );
                }
            },
            version: crate::approval::digest(&details),
            details,
        },
        title: format!("Confirm profile {}", preview.metadata.profile),
        reason: "Save the reviewed profile change; restart is required.".into(),
        persistent: true,
        blocking: false,
        expires_at: Some(chrono::Utc::now().timestamp() + CHANGE_TTL.as_secs() as i64),
    }) {
        Ok(request) => request,
        Err(error) => {
            return error_response(StatusCode::CONFLICT, "approval_failed", &error.to_string());
        }
    };
    let id = approval.id;
    let response = ChangeResponse {
        id: id.clone(),
        expires_in_seconds: CHANGE_TTL.as_secs(),
        operation: preview.metadata.operation.to_string(),
        profile: preview.metadata.profile.clone(),
        field: preview
            .metadata
            .field
            .map(|field| format!("{field:?}").to_lowercase()),
        diff: preview.redacted_diff.clone(),
    };
    server.logger().info(
        "config_change_previewed",
        &[
            ("request_id", request_id),
            ("operation", response.operation.clone()),
            ("profile", response.profile.clone()),
        ],
    );
    json_response(StatusCode::OK, &response)
}

pub async fn confirm(
    Path(id): Path<String>,
    State(server): State<std::sync::Arc<Server>>,
    headers: HeaderMap,
) -> Response {
    let request_id = correlation(&headers);
    let Some(service) = &server.approvals else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "approval_unavailable",
            "approval service unavailable",
        );
    };
    let request = match service.get(&id, &server.info.workspace, None) {
        Ok(request)
            if request.subject.source == "config"
                && request.state == otto_core::approval::State::Pending =>
        {
            request
        }
        _ => {
            return error_response(
                StatusCode::NOT_FOUND,
                "change_not_found",
                "configuration change was not found",
            );
        }
    };
    if request
        .expires_at
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp())
    {
        return error_response(
            StatusCode::GONE,
            "change_expired",
            "configuration change expired",
        );
    }
    if let Err(error) = service.decide(
        &id,
        &request.scope,
        request.revision,
        true,
        "",
        &crate::approval::Actor::local("config-confirm"),
    ) {
        return error_response(StatusCode::CONFLICT, "approval_failed", &error.to_string());
    }
    if let Err(error) = service.apply_available(&server.info.workspace) {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "approval_unavailable",
            &error.to_string(),
        );
    }
    match service.get(&id, &server.info.workspace, None) {
        Ok(request) if request.application == otto_core::approval::ApplicationState::Applied => {
            server.logger().info(
                "config_change_confirmed",
                &[("request_id", request_id), ("approval_id", id)],
            );
            json_response(
                StatusCode::OK,
                &serde_json::json!({"status":"saved_restart_required"}),
            )
        }
        _ => error_response(
            StatusCode::CONFLICT,
            "change_stale",
            "configuration change could not be applied; review the current file",
        ),
    }
}

pub async fn cancel(
    Path(id): Path<String>,
    State(server): State<std::sync::Arc<Server>>,
) -> Response {
    let Some(service) = &server.approvals else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "approval_unavailable",
            "approval service unavailable",
        );
    };
    match service.get(&id, &server.info.workspace, None) {
        Ok(request) if request.subject.source == "config" => {
            match service.withdraw(&id, &request.scope, request.revision) {
                Ok(_) => json_response(StatusCode::NO_CONTENT, &serde_json::json!({})),
                Err(error) => {
                    error_response(StatusCode::CONFLICT, "approval_failed", &error.to_string())
                }
            }
        }
        _ => error_response(
            StatusCode::NOT_FOUND,
            "change_not_found",
            "configuration change was not found",
        ),
    }
}

#[cfg(test)]
mod claude_tests {
    use super::*;
    #[test]
    fn claude_provider_and_connection_fields_are_editable() {
        assert!(matches!(provider("claude"), Ok(ProfileProvider::Claude)));
        assert!(matches!(field("api_version"), Ok(ProfileField::ApiVersion)));
        assert!(matches!(
            field("workspace_id_env"),
            Ok(ProfileField::WorkspaceIdEnv)
        ));
        let file = otto_core::config::parse("[profiles.claude]\nprovider='claude'\nmodel='fixture'\napi_version='2026-10-21'\nworkspace_id_env='WORKSPACE_ID'").unwrap();
        let value = serde_json::to_value(profile("claude", &file).unwrap()).unwrap();
        assert_eq!(value["api_version"], "2026-10-21");
        assert_eq!(value["workspace_id_env"], "WORKSPACE_ID");
    }
}
