//! Keeps `examples/config.example.toml` loadable: it parses without legacy
//! diagnostics and every model it defines resolves.

use std::collections::HashMap;

use super::resolve::resolve_without_credentials;
use super::{Overrides, SessionDefaults, parse};

const SAMPLE_CONFIG_TOML: &str = include_str!("../../../../examples/config.example.toml");

#[test]
fn sample_config_parses_without_diagnostics() {
    let file = parse(SAMPLE_CONFIG_TOML).expect("sample config parses");
    assert!(file.diagnostics.is_empty(), "{:?}", file.diagnostics);
    assert_eq!(file.default_model, "chatgpt/gpt-5.6-sol");
    assert_eq!(file.providers["local"].type_, "openai-compatible");
    assert_eq!(file.ui.observer, super::ui::ObserverBindings::default());
}

#[test]
fn every_sample_model_resolves() {
    let file = parse(SAMPLE_CONFIG_TOML).expect("sample config parses");
    let mut selectors = Vec::new();
    for (provider_name, provider) in &file.providers {
        for model_id in provider.models.keys() {
            selectors.push(format!("{provider_name}/{model_id}"));
        }
    }
    assert_eq!(selectors.len(), 6, "{selectors:?}");
    for selector in selectors {
        let overrides = Overrides {
            model: selector.clone(),
            ..Overrides::default()
        };
        let runtime = resolve_without_credentials(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &overrides,
        )
        .unwrap_or_else(|err| panic!("{selector}: {err}"));
        assert_eq!(runtime.model_selector.to_string(), selector);
    }
}
