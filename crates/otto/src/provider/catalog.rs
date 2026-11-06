//! Bounded, on-demand provider catalogs. Metadata is untrusted provider data,
//! not instructions, verified capabilities, or a price/performance ranking.
use std::time::Duration;

use futures_util::TryStreamExt;
use otto_core::provider::ProviderError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

pub const TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_BODY: usize = 8 << 20;
const MAX_ID: usize = 512;
const MAX_METADATA: usize = 2048;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelCatalog {
    pub models: Vec<CatalogModel>,
    /// Entries or metadata omitted by validation/size bounds.
    pub omitted: usize,
}

pub(crate) fn failure(message: &str) -> ProviderError {
    ProviderError::Other(message.to_owned())
}

/// Uses a separate no-redirect client even when completion transport was injected.
pub(crate) fn http() -> Result<reqwest::Client, ProviderError> {
    super::endpoint_dns::http_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(TIMEOUT)
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| failure("model catalog client unavailable"))
}

/// No provider error body or reqwest diagnostic is exposed to callers.
pub(crate) async fn read(request: reqwest::RequestBuilder) -> Result<Vec<u8>, ProviderError> {
    let response = request
        .send()
        .await
        .map_err(|_| failure("model catalog request failed"))?;
    if !response.status().is_success() {
        return Err(failure(&format!(
            "model catalog HTTP {}; check provider access or sign in again",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return Err(failure("model catalog exceeds response size limit"));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|_| failure("model catalog body read failed"))?
    {
        if chunk.len() > MAX_BODY.saturating_sub(bytes.len()) {
            return Err(failure("model catalog exceeds response size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The deadline covers authentication, send and every body read together.
pub(crate) async fn controlled<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ProviderError::Cancelled),
        result = tokio::time::timeout(TIMEOUT, future) => result.unwrap_or_else(|_| Err(failure("model catalog timed out; retry or use the configured model"))),
    }
}

/// Remove credential-bearing IDs rather than offering fabricated replacements.
pub(crate) fn sanitize(catalog: &mut ModelCatalog, secrets: &[&str]) {
    let before = catalog.models.len();
    catalog
        .models
        .retain(|m| !secrets.iter().any(|s| !s.is_empty() && m.id.contains(s)));
    catalog.omitted += before - catalog.models.len();
    fn scrub(value: &mut Value, secrets: &[&str]) {
        match value {
            Value::String(s) => {
                for secret in secrets.iter().filter(|s| !s.is_empty()) {
                    *s = s.replace(secret, "[REDACTED]");
                }
            }
            Value::Array(a) => {
                for v in a {
                    scrub(v, secrets);
                }
            }
            Value::Object(o) => {
                let old = std::mem::take(o);
                for (mut key, mut value) in old {
                    for secret in secrets.iter().filter(|s| !s.is_empty()) {
                        key = key.replace(secret, "[REDACTED]");
                    }
                    scrub(&mut value, secrets);
                    o.insert(key, value);
                }
            }
            _ => {}
        }
    }
    for model in &mut catalog.models {
        for value in model.metadata.values_mut() {
            scrub(value, secrets);
        }
    }
}

pub(crate) fn parse(bytes: &[u8], chatgpt: bool) -> Result<ModelCatalog, ProviderError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| failure("decode model list: invalid catalog JSON"))?;
    let rows = value
        .get(if chatgpt { "models" } else { "data" })
        .and_then(Value::as_array)
        .ok_or_else(|| failure("decode model list: missing model array"))?;
    let mut catalog = ModelCatalog::default();
    for row in rows {
        // Codex openai_models.rs:1022-1028 keeps supported_in_api=false in
        // ChatGPT mode: that flag describes API-key access, not subscriptions.
        if (chatgpt
            && matches!(
                row.get("visibility").and_then(Value::as_str),
                Some("hide" | "none")
            ))
            || ["input_modalities", "output_modalities"].iter().any(|key| {
                row.get(*key)
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.iter().any(|x| x.as_str() == Some("text")))
            })
        {
            catalog.omitted += 1;
            continue;
        }
        let Some(id) = row
            .get(if chatgpt { "slug" } else { "id" })
            .and_then(Value::as_str)
        else {
            catalog.omitted += 1;
            continue;
        };
        if id.is_empty()
            || id.len() > MAX_ID
            || id.chars().any(char::is_whitespace)
            || id.chars().any(char::is_control)
        {
            catalog.omitted += 1;
            continue;
        }
        let mut metadata = Map::new();
        for key in [
            "display_name",
            "description",
            "input_modalities",
            "output_modalities",
            "context_window",
            "supported_in_api",
            "supports_parallel_tool_calls",
            "supports_reasoning_summaries",
            "supports_reasoning_summary_parameter",
            "shell_type",
            "apply_patch_tool_type",
            "experimental_supported_tools",
            "supports_search_tool",
            "supported_reasoning_levels",
        ] {
            if let Some(value) = row.get(key).filter(|v| !v.is_null()) {
                if serde_json::to_vec(value).is_ok_and(|v| v.len() <= MAX_METADATA) {
                    metadata.insert(key.to_owned(), value.clone());
                } else {
                    catalog.omitted += 1;
                }
            }
        }
        catalog.models.push(CatalogModel {
            id: id.to_owned(),
            metadata,
        });
    }
    catalog.models.sort_by(|a, b| a.id.cmp(&b.id));
    catalog.models.dedup_by(|a, b| a.id == b.id);
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_parser_preserves_metadata_without_ranking() {
        let c = parse(
            b"{\"models\":[{\"slug\":\"subscription-model\",\"supported_in_api\":false,\
                \"input_modalities\":[\"text\"],\"description\":\"untrusted\"},\
                {\"slug\":\"hidden\",\"visibility\":\"hide\"},\
                {\"slug\":\"audio\",\"input_modalities\":[\"audio\"]},\
                {\"slug\":\"bad\\nid\"}]}",
            true,
        )
        .unwrap();
        assert_eq!(c.models.len(), 1);
        assert_eq!(c.models[0].metadata["supported_in_api"], false);
        assert_eq!(c.omitted, 3);
    }
    #[tokio::test]
    async fn catalog_transport_rejects_redirects_errors_and_oversize_without_body_leaks() {
        use crate::faults::http as fixture;
        let target = fixture::spawn(|_| fixture::json_response(r#"{"data":[]}"#)).await;
        let location = target.url.clone();
        let redirect = fixture::spawn(move |_| fixture::redirect_response(307, &location)).await;
        let error = read(http().unwrap().get(&redirect.url)).await.unwrap_err();
        assert!(error.to_string().contains("307"));
        assert_eq!(target.count(), 0);
        let origin =
            fixture::spawn(|_| fixture::status_response(503, "provider-body-secret")).await;
        let error = read(http().unwrap().get(&origin.url)).await.unwrap_err();
        assert!(!error.to_string().contains("provider-body-secret"));
        let oversized = fixture::spawn(|_| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY + 1
            )
        })
        .await;
        assert!(
            read(http().unwrap().get(&oversized.url))
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
    }

    #[tokio::test]
    async fn catalog_body_read_observes_cancellation() {
        use crate::faults::http as fixture;
        let origin = fixture::spawn_repeating(fixture::Script::stall_after_head()).await;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            stop.cancel();
        });
        let result = controlled(&cancel, read(http().unwrap().get(&origin.url))).await;
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[test]
    fn catalog_sanitizer_omits_secret_ids_and_redacts_nested_metadata() {
        let mut c = parse(br#"{"data":[{"id":"secret-model"},{"id":"valid","description":"secret","supported_reasoning_levels":[{"secret":"secret"}]}]}"#, false).unwrap();
        sanitize(&mut c, &["secret"]);
        assert_eq!(c.models.len(), 1);
        assert_eq!(c.omitted, 1);
        assert!(!serde_json::to_string(&c).unwrap().contains("secret"));
    }

    #[tokio::test(start_paused = true)]
    async fn catalog_total_timeout_bounds_pending_auth_or_io() {
        let error = controlled::<()>(&CancellationToken::new(), std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn compat_ids_need_no_metadata() {
        let c = parse(br#"{"data":[{"id":"z"},{"id":"a"},{"id":"z"}]}"#, false).unwrap();
        assert_eq!(
            c.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "z"]
        );
        assert!(parse(br#"{"models":[]}"#, false).is_err());
    }
    #[tokio::test]
    async fn cancellation_preempts_pending_work() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            controlled::<()>(&cancel, std::future::pending()).await,
            Err(ProviderError::Cancelled)
        ));
    }
}
