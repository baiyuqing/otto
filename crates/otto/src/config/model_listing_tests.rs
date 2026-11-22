use super::{configured_model_selectors, initial_config_content};
use otto_core::config::{File, parse};

#[test]
fn effective_picker_is_sorted_and_keeps_explicit_models_and_hidden_default() {
    let empty = configured_model_selectors(&File::default());
    assert!(
        empty
            .iter()
            .any(|selector| selector == "deepseek/deepseek-v4-pro")
    );
    let file = parse(
        "default_model='deepseek/deepseek-flash'\n\
        [providers.deepseek]\ninclude_preset_models=false\nmodels.custom={}\n\
        [providers.chatgpt.models.'gpt-6-luna']\nthinking='high'\n\
        [providers.chatgpt.models.'abc']\n\
        [providers.alias]\ntype='chatgpt'\nmodels.extra={}",
    )
    .unwrap();
    let selectors = configured_model_selectors(&file);
    assert!(selectors.windows(2).all(|pair| pair[0] < pair[1]));
    for selector in [
        "deepseek/deepseek-flash",
        "deepseek/custom",
        "chatgpt/gpt-6-luna",
        "chatgpt/abc",
        "alias/extra",
    ] {
        assert!(selectors.iter().any(|item| item == selector));
    }
    assert!(
        !selectors
            .iter()
            .any(|item| item == "deepseek/deepseek-v4-pro" || item == "alias/gpt-6-luna")
    );
}

#[test]
fn deepseek_setup_uses_only_the_selector_and_defaults() {
    let content = initial_config_content("deepseek", "deepseek-flash", "", "").unwrap();
    let file = parse(&content).unwrap();
    assert_eq!(file.default_model, "deepseek/deepseek-flash");
    assert!(file.providers.is_empty());
    assert!(initial_config_content("deepseek", "", "", "").is_err());
    assert!(
        initial_config_content("deepseek", "deepseek-flash", "https://other.test", "").is_err()
    );
}
