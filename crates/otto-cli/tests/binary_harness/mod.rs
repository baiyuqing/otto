//! Helpers shared by the tests that drive the built `otto` binary: the PTY
//! screen scraper and the ACP tool-call assertion. The scripted provider they
//! talk to is `otto::fixtures::scripted_provider`.

// Each test binary compiles its own copy, so some helpers are unused in some.
#![allow(dead_code)]

#[cfg(unix)]
pub mod job_control;
#[cfg(unix)]
pub mod pty;

/// A displayed ACP tool call must close exactly once, including on failure.
pub fn assert_terminal_tool_update(frames: &[serde_json::Value], id: &str, status: &str) {
    let updates: Vec<_> = frames
        .iter()
        .filter(|frame| frame["method"] == "session/update")
        .map(|frame| &frame["params"]["update"])
        .filter(|update| update["toolCallId"] == id)
        .collect();
    assert_eq!(
        updates
            .iter()
            .filter(|update| update["sessionUpdate"] == "tool_call")
            .count(),
        1,
        "{updates:?}"
    );
    let terminal: Vec<_> = updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "tool_call_update")
        .collect();
    assert_eq!(terminal.len(), 1, "{updates:?}");
    assert_eq!(terminal[0]["status"], status, "{updates:?}");
}

/// One explicitly configured offline provider, granting only its exact origin.
pub fn provider_config(name: &str, base_url: &str, model: &str, api_key_env: &str) -> String {
    let url = reqwest::Url::parse(base_url).expect("fixture provider URL");
    let origin = url.origin().ascii_serialization();
    let http_grant = if url.scheme() == "http" {
        format!("allow_http_origins = [{origin:?}]\n")
    } else {
        String::new()
    };
    format!(
        "[providers.{name}]\ntype = \"openai-compatible\"\nbase_url = {base_url:?}\n\
         api_key_env = {api_key_env:?}\ntrusted_origins = [{origin:?}]\n{http_grant}\
         [providers.{name}.models.{model}]\n"
    )
}

/// A selected fixture model with explicit user-owned endpoint grants.
pub fn model_config(base_url: &str, api_key_env: &str) -> String {
    format!(
        "default_model = \"test/gpt-test\"\n\n{}",
        provider_config("test", base_url, "gpt-test", api_key_env)
    )
}
