//! Restricted profile configuration management routes.
//!
//! These handlers accept typed profile operations only. They never expose raw
//! TOML, environment values, or credentials; writes go through the native
//! configuration module's preview and CAS-backed commit path.

use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::config::{
    self as native, ProfileChange, ProfileChangePreview, ProfileField, ProfileProvider,
};
use crate::provider::openaicompat::Client as OpenAiClient;

use super::{Server, error_response, json_response};

const CHANGE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_MODEL_IDS: usize = 2_000;

fn correlation(headers: &HeaderMap) -> String {
    headers
        .get("x-otto-request-id")
        .map(|value| super::request_id(value.as_bytes()))
        .unwrap_or_default()
}

pub(super) struct PendingChange {
    pub expires: Instant,
    pub preview: ProfileChangePreview,
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
    if profile.provider != otto_core::config::PROVIDER_OPENAI_COMPATIBLE {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "profile has an unsupported provider",
        );
    }
    let api_key = match std::env::var(&profile.api_key_env) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "api_key_unavailable",
                "the profile API-key environment variable is unavailable",
            );
        }
    };
    let client = OpenAiClient::new(&profile.base_url, &api_key);
    match client
        .list_models(&tokio_util::sync::CancellationToken::new())
        .await
    {
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
        _ => Err(Box::new(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "provider must be openai-compatible or chatgpt",
        ))),
    }
}
fn field(value: &str) -> Result<ProfileField, Box<Response>> {
    match value {
        "model" => Ok(ProfileField::Model),
        "thinking" => Ok(ProfileField::Thinking),
        "base_url" => Ok(ProfileField::BaseUrl),
        "api_key_env" => Ok(ProfileField::ApiKeyEnv),
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

fn token() -> Result<String, Box<Response>> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "random_unavailable",
                "cannot create confirmation token",
            )
        })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
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
    let id = match token() {
        Ok(id) => id,
        Err(reply) => return *reply,
    };
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
    let mut pending = server
        .config_changes
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    pending.retain(|_, change| change.expires > Instant::now());
    pending.insert(
        id,
        PendingChange {
            expires: Instant::now() + CHANGE_TTL,
            preview,
        },
    );
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
    let pending = server
        .config_changes
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&id);
    let Some(pending) = pending else {
        return error_response(
            StatusCode::NOT_FOUND,
            "change_not_found",
            "configuration change was not found",
        );
    };
    if pending.expires <= Instant::now() {
        return error_response(
            StatusCode::GONE,
            "change_expired",
            "configuration change expired; submit it again",
        );
    }
    match native::commit_profile_change(&server.config_path, &pending.preview) {
        Ok(()) => {
            server.logger().info(
                "config_change_confirmed",
                &[
                    ("request_id", request_id),
                    ("operation", pending.preview.metadata.operation.to_string()),
                    ("profile", pending.preview.metadata.profile),
                ],
            );
            json_response(
                StatusCode::OK,
                &serde_json::json!({"status":"saved_restart_required"}),
            )
        }
        Err(error) => error_response(StatusCode::CONFLICT, "change_stale", &error.to_string()),
    }
}

pub async fn cancel(
    Path(id): Path<String>,
    State(server): State<std::sync::Arc<Server>>,
) -> Response {
    let removed = server
        .config_changes
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&id);
    if removed.is_some() {
        json_response(StatusCode::NO_CONTENT, &serde_json::json!({}))
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            "change_not_found",
            "configuration change was not found",
        )
    }
}
