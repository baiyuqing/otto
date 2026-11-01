//! The model selectors a configuration names.

use otto_core::config::{File, ModelSelector};

/// Every `<provider>/<model_id>` selector with a model table, plus
/// `default_model` when it parses; sorted and without duplicates.
pub fn configured_model_selectors(file: &File) -> Vec<String> {
    let mut selectors: Vec<String> = file
        .providers
        .iter()
        .flat_map(|(provider, entry)| {
            entry.models.keys().filter_map(|model_id| {
                ModelSelector::from_parts(provider, model_id)
                    .ok()
                    .map(|selector| selector.to_string())
            })
        })
        .collect();
    if let Ok(default) = ModelSelector::parse(&file.default_model) {
        selectors.push(default.to_string());
    }
    selectors.sort();
    selectors.dedup();
    selectors
}
