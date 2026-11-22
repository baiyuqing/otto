use std::collections::HashMap;

use super::endpoint::EndpointRoute;
use super::providers::{DEEPSEEK_DEFAULT_API_KEY_ENV, DEEPSEEK_DEFAULT_BASE_URL};
use super::{File, Overrides, parse, resolve};

#[test]
fn model_presets_overlay_preserves_raw_config_and_explicit_entries() {
    let file = parse(
        "[providers.chatgpt.models.'gpt-6-luna']\nthinking='high'\n\
         [providers.deepseek]\ninclude_preset_models=false\n\
         [providers.deepseek.models.deepseek-flash]\ncontext_window=100000",
    )
    .unwrap();
    let chatgpt = file.provider_entry("chatgpt").unwrap();
    assert_eq!(chatgpt.models.len(), 7);
    assert_eq!(chatgpt.models["gpt-6-luna"].thinking, "high");
    assert!(chatgpt.models.contains_key("gpt-5.6-terra"));
    assert_eq!(file.providers["chatgpt"].models.len(), 1);
    let deepseek = file.provider_entry("deepseek").unwrap();
    assert_eq!(deepseek.models.len(), 1);
    assert_eq!(
        deepseek.models["deepseek-flash"].context_window,
        Some(100000)
    );
    let claude = file.provider_entry("claude").unwrap();
    assert_eq!(
        claude.models.keys().map(String::as_str).collect::<Vec<_>>(),
        ["claude-haiku-5-5", "claude-opus-4-8", "claude-sonnet-4-6"]
    );
    assert!(File::default().default_model.is_empty());
    assert!(parse("[providers.deepseek]\ntype='deepseek'").is_err());
    assert!(parse("[providers.other]\ntype='deepseek'").is_err());
}

#[test]
fn deepseek_endpoint_route_and_implicit_credential_are_origin_scoped() {
    let file = parse(
        "default_model='deepseek/deepseek-flash'\n\
         [providers.deepseek]\ntype='openai-compatible'\n\
         trusted_origins=['https://api.deepseek.com:444','https://api.deepseek.com.invalid']",
    )
    .unwrap();
    let env = HashMap::from([(DEEPSEEK_DEFAULT_API_KEY_ENV.into(), "fixture-value".into())]);
    let resolve_url = |base_url: &str| {
        resolve(
            &file,
            &env,
            &Default::default(),
            &Overrides {
                base_url: base_url.into(),
                ..Default::default()
            },
        )
    };
    let official = resolve_url("").unwrap();
    assert_eq!(official.provider_type, "openai-compatible");
    assert_eq!(official.base_url, DEEPSEEK_DEFAULT_BASE_URL);
    assert_eq!(
        official.endpoint.unwrap().route(),
        EndpointRoute::OfficialDeepSeek
    );
    assert_eq!(official.api_key_env, DEEPSEEK_DEFAULT_API_KEY_ENV);
    let explicit_default_port = resolve_url("https://api.deepseek.com:443/v1/").unwrap();
    assert_eq!(
        explicit_default_port.endpoint.unwrap().route(),
        EndpointRoute::OfficialDeepSeek
    );
    for url in [
        "https://api.deepseek.com:444",
        "https://api.deepseek.com.invalid",
    ] {
        let custom = resolve_url(url).unwrap();
        assert_eq!(custom.endpoint.unwrap().route(), EndpointRoute::Standard);
        assert!(custom.api_key_env.is_empty());
        assert!(custom.api_key.is_empty());
    }
    for url in [
        "http://api.deepseek.com",
        "https://api.openai.com",
        "https://api.deepseek.com?x=y",
    ] {
        assert!(resolve_url(url).is_err());
    }
    let other = parse(
        "default_model='other/deepseek-flash'\n[providers.other]\n\
        type='openai-compatible'\nbase_url='https://api.deepseek.com'\n\
        trusted_origins=['https://api.deepseek.com']",
    )
    .unwrap();
    let other = resolve(&other, &env, &Default::default(), &Default::default()).unwrap();
    assert_eq!(other.endpoint.unwrap().route(), EndpointRoute::Standard);
}
