//! The effective offline model picker.

use std::collections::BTreeSet;

use otto_core::config::providers::BUILTIN_PROVIDERS;
use otto_core::config::{File, ModelSelector};

/// Static presets and explicit model tables, plus a valid `default_model`.
/// Results are sorted and deduplicated without remote discovery.
pub fn configured_model_selectors(file: &File) -> Vec<String> {
    let providers: BTreeSet<_> = BUILTIN_PROVIDERS
        .into_iter()
        .chain(file.providers.keys().map(String::as_str))
        .collect();
    let mut selectors = BTreeSet::new();
    for provider in providers {
        if let Some(entry) = file.provider_entry(provider) {
            selectors.extend(entry.models.keys().filter_map(|model_id| {
                ModelSelector::from_parts(provider, model_id)
                    .ok()
                    .map(|model| model.to_string())
            }));
        }
    }
    if let Ok(default) = ModelSelector::parse(&file.default_model) {
        selectors.insert(default.to_string());
    }
    selectors.into_iter().collect()
}
