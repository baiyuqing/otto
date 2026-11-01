//! Restricted provider and model configuration management routes.
//!
//! These handlers accept typed provider and model operations only. They never expose raw
//! TOML, environment values, or credentials; writes go through the native
//! configuration module's preview and CAS-backed commit path.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::config::{
    self as native, ModelChange, ModelChangeError, ModelField, ProviderField, ProviderType,
};
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
struct ModelResponse {
    model_id: String,
    selector: String,
    default: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_window: Option<i64>,
}

#[derive(Serialize)]
struct ProviderResponse {
    name: String,
    #[serde(rename = "type")]
    provider_type: String,
    builtin: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    base_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    api_key_env: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    api_version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    workspace_id_env: String,
    models: Vec<ModelResponse>,
}

/// The provider `name` with its connection fields and model tables, or `None`
/// when the name is neither configured nor built in. The model named by
/// `default_model` is listed even without a table.
fn provider_response(name: &str, file: &otto_core::config::File) -> Option<ProviderResponse> {
    let entry = file.provider_entry(name)?;
    let default_selector = otto_core::config::ModelSelector::parse(&file.default_model).ok();
    let mut model_ids: Vec<String> = entry.models.keys().cloned().collect();
    if let Some(selector) = default_selector
        .as_ref()
        .filter(|selector| selector.provider == name)
        && !model_ids.contains(&selector.model_id)
    {
        model_ids.push(selector.model_id.clone());
        model_ids.sort();
    }
    let models = model_ids
        .into_iter()
        .map(|model_id| {
            let model = entry.models.get(&model_id).cloned().unwrap_or_default();
            let selector = format!("{name}/{model_id}");
            ModelResponse {
                default: file.default_model == selector,
                selector,
                model_id,
                thinking: model.thinking,
                context_window: model.context_window,
                compaction_window: model.compaction_window,
            }
        })
        .collect();
    Some(ProviderResponse {
        name: name.to_string(),
        provider_type: entry.provider_type(name).to_string(),
        builtin: otto_core::config::providers::is_builtin_provider(name),
        base_url: entry.base_url.clone(),
        api_key_env: entry.api_key_env.clone(),
        api_version: entry.api_version.clone(),
        workspace_id_env: entry.workspace_id_env.clone(),
        models,
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

pub async fn providers(State(server): State<std::sync::Arc<Server>>) -> Response {
    let file = match load(&server) {
        Ok(file) => file,
        Err(reply) => return *reply,
    };
    let mut names: Vec<String> = otto_core::config::providers::BUILTIN_PROVIDERS
        .iter()
        .map(|name| name.to_string())
        .collect();
    names.extend(file.providers.keys().cloned());
    names.sort();
    names.dedup();
    let providers: Vec<_> = names
        .iter()
        .filter_map(|name| provider_response(name, &file))
        .collect();
    json_response(
        StatusCode::OK,
        &serde_json::json!({"default_model": file.default_model, "providers": providers}),
    )
}

pub async fn get_provider(
    Path(name): Path<String>,
    State(server): State<std::sync::Arc<Server>>,
) -> Response {
    let file = match load(&server) {
        Ok(file) => file,
        Err(reply) => return *reply,
    };
    match provider_response(&name, &file) {
        Some(provider) => json_response(StatusCode::OK, &provider),
        None => error_response(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "provider not found",
        ),
    }
}

#[derive(Deserialize)]
pub struct ModelsQuery {
    provider: String,
}

pub async fn models(
    Query(query): Query<ModelsQuery>,
    State(server): State<std::sync::Arc<Server>>,
) -> Response {
    let file = match load(&server) {
        Ok(file) => file,
        Err(reply) => return *reply,
    };
    let Some(entry) = file.provider_entry(&query.provider) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            "provider not found",
        );
    };
    let provider_type = entry.provider_type(&query.provider).to_string();
    if provider_type == otto_core::config::PROVIDER_CHATGPT {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "models_unsupported",
            "the chatgpt provider cannot list models",
        );
    }
    if !matches!(
        provider_type.as_str(),
        otto_core::config::PROVIDER_OPENAI_COMPATIBLE | otto_core::config::PROVIDER_CLAUDE
    ) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "provider has an unsupported type",
        );
    }
    // Listing needs only the provider connection; the model ID resolves the
    // connection and is not sent to the provider.
    let connection_model_id = entry
        .models
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "list-models".to_string());
    let environment: std::collections::HashMap<String, String> = std::env::vars().collect();
    let runtime = match otto_core::config::resolve(
        &file,
        &environment,
        &otto_core::config::SessionDefaults::default(),
        &otto_core::config::Overrides {
            model: format!("{}/{connection_model_id}", query.provider),
            ..Default::default()
        },
    ) {
        Ok(runtime) => runtime,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "api_key_unavailable",
                "provider connection or credentials are unavailable; check the provider and its environment bindings",
            );
        }
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = if runtime.provider_type == otto_core::config::PROVIDER_CLAUDE {
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
                &serde_json::json!({"provider": query.provider, "models": ids}),
            )
        }
        Err(error) => error_response(StatusCode::BAD_GATEWAY, "models_failed", &error.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ChangeRequest {
    SetDefaultModel {
        model: String,
    },
    SetProvider {
        provider: String,
        #[serde(rename = "type")]
        provider_type: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        api_version: Option<String>,
        #[serde(default)]
        workspace_id_env: Option<String>,
    },
    SetProviderField {
        provider: String,
        field: String,
        value: String,
    },
    RemoveProvider {
        provider: String,
    },
    SetModel {
        model: String,
        #[serde(default)]
        thinking: Option<String>,
    },
    SetModelField {
        model: String,
        field: String,
        value: String,
    },
    RemoveModel {
        model: String,
    },
}

fn provider_type(value: &str) -> Result<ProviderType, Box<Response>> {
    match value {
        otto_core::config::PROVIDER_OPENAI_COMPATIBLE => Ok(ProviderType::OpenAiCompatible),
        otto_core::config::PROVIDER_CHATGPT => Ok(ProviderType::ChatGpt),
        otto_core::config::PROVIDER_CLAUDE => Ok(ProviderType::Claude),
        _ => Err(Box::new(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "type must be openai-compatible, claude or chatgpt",
        ))),
    }
}

fn provider_field(value: &str) -> Result<ProviderField, Box<Response>> {
    match value {
        "base_url" => Ok(ProviderField::BaseUrl),
        "api_key_env" => Ok(ProviderField::ApiKeyEnv),
        "api_version" => Ok(ProviderField::ApiVersion),
        "workspace_id_env" => Ok(ProviderField::WorkspaceIdEnv),
        _ => Err(Box::new(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            "field is not editable",
        ))),
    }
}

fn model_field(value: &str) -> Result<ModelField, Box<Response>> {
    match value {
        "thinking" => Ok(ModelField::Thinking),
        _ => Err(Box::new(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_field",
            "field is not editable",
        ))),
    }
}

fn change(request: ChangeRequest) -> Result<ModelChange, Box<Response>> {
    Ok(match request {
        ChangeRequest::SetDefaultModel { model } => ModelChange::SetDefaultModel { model },
        ChangeRequest::SetProvider {
            provider,
            provider_type: type_name,
            base_url,
            api_key_env,
            api_version,
            workspace_id_env,
        } => ModelChange::SetProvider {
            provider,
            provider_type: provider_type(&type_name)?,
            base_url,
            api_key_env,
            api_version,
            workspace_id_env,
        },
        ChangeRequest::SetProviderField {
            provider,
            field,
            value,
        } => ModelChange::SetProviderField {
            provider,
            field: provider_field(&field)?,
            value,
        },
        ChangeRequest::RemoveProvider { provider } => ModelChange::RemoveProvider { provider },
        ChangeRequest::SetModel { model, thinking } => ModelChange::SetModel { model, thinking },
        ChangeRequest::SetModelField {
            model,
            field,
            value,
        } => ModelChange::SetModelField {
            model,
            field: model_field(&field)?,
            value,
        },
        ChangeRequest::RemoveModel { model } => ModelChange::RemoveModel { model },
    })
}

#[derive(Serialize)]
struct ChangeResponse {
    id: String,
    expires_in_seconds: u64,
    operation: String,
    target: String,
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
    let preview = match native::preview_model_change(&server.config_path, &change) {
        Ok(preview) => preview,
        Err(ModelChangeError::InUse(message)) => {
            return error_response(StatusCode::CONFLICT, "model_in_use", &message);
        }
        Err(ModelChangeError::LegacyProfiles(message)) => {
            return error_response(StatusCode::CONFLICT, "legacy_profiles_config", &message);
        }
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
    let artifact = crate::approval::model_change::Artifact::prepare(change, &preview);
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
        title: format!(
            "Confirm model configuration change for {}",
            preview.metadata.target
        ),
        reason: "Save the reviewed provider or model change; restart is required.".into(),
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
        target: preview.metadata.target.clone(),
        field: preview.metadata.field.map(str::to_string),
        diff: preview.redacted_diff.clone(),
    };
    server.logger().info(
        "config_change_previewed",
        &[
            ("request_id", request_id),
            ("operation", response.operation.clone()),
            ("target", response.target.clone()),
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
mod provider_shape_tests {
    use super::*;

    #[test]
    fn provider_response_lists_connection_fields_and_marks_the_default_model() {
        let file = otto_core::config::parse(
            "default_model='claude/fixture'\n[providers.claude]\napi_version='2026-10-21'\nworkspace_id_env='WORKSPACE_ID'\n\
             [providers.claude.models.fixture]\nthinking='high'\n[providers.claude.models.other]\n",
        )
        .unwrap();
        let value = serde_json::to_value(provider_response("claude", &file).unwrap()).unwrap();
        assert_eq!(value["type"], "claude");
        assert_eq!(value["builtin"], true);
        assert_eq!(value["api_version"], "2026-10-21");
        assert_eq!(value["workspace_id_env"], "WORKSPACE_ID");
        assert!(value.get("base_url").is_none());
        assert_eq!(value["models"][0]["selector"], "claude/fixture");
        assert_eq!(value["models"][0]["default"], true);
        assert_eq!(value["models"][0]["thinking"], "high");
        assert_eq!(value["models"][1]["model_id"], "other");
        assert_eq!(value["models"][1]["default"], false);
        assert!(provider_response("unknown", &file).is_none());
    }

    #[test]
    fn change_fields_are_restricted_to_connection_and_thinking() {
        assert!(matches!(provider_type("claude"), Ok(ProviderType::Claude)));
        assert!(provider_type("other").is_err());
        assert!(matches!(
            provider_field("api_version"),
            Ok(ProviderField::ApiVersion)
        ));
        assert!(provider_field("models").is_err());
        assert!(matches!(model_field("thinking"), Ok(ModelField::Thinking)));
        assert!(model_field("context_window").is_err());
    }
}
