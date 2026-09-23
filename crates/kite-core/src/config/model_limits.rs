//! The context-window catalog and lookup.

const OPENAI_MODEL_SOURCE_URL: &str = "https://developers.openai.com/api/docs/models/all";
const ANTHROPIC_MODEL_SOURCE_URL: &str = "https://platform.claude.com/docs/en/models/overview";

/// The context-window family a model ID resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    GptLong,
    Gpt400K,
    GptChat,
    GptSpark,
    Gpt41,
    Gpt4o,
    Gpt4o4096,
    OSeries,
    ClaudeMillion128,
    ClaudeMillion64,
    Claude200K64,
    Claude200K32,
    Claude200K8192,
    Claude200K4096,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelNamespace {
    None,
    OpenAI,
    Anthropic,
}

/// The resolved context-window limits for a model, or all-zero/unknown when
/// the model ID isn't in the catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelLimits {
    pub known: bool,
    pub context_window: i64,
    pub hard_input_window: i64,
    pub max_output_tokens: i64,
    pub working_window: i64,
    pub source_url: String,
}

/// Looks up `model`'s context-window limits. Unknown models (including
/// ambiguous namespace prefixes and malformed `:batch` suffixes) return a
/// zeroed, `known: false` [`ModelLimits`].
pub(super) fn resolve_model_limits(model: &str) -> ModelLimits {
    let Some((model, namespace)) = unwrap_model_id(model) else {
        return ModelLimits::default();
    };
    let model = model.as_str();

    if namespace != ModelNamespace::Anthropic {
        if let Some(family) = exact_snapshot_family(model) {
            return limits_for_family(family);
        }
        if let Some(family) = openai_alias_family(model) {
            return limits_for_family(family);
        }
        if let Some(prefix) = strip_openai_date_suffix(model)
            && let Some(family) = openai_alias_family(prefix)
        {
            return limits_for_family(family);
        }
    }

    if namespace != ModelNamespace::OpenAI {
        if let Some(family) = anthropic_alias_family(model) {
            return limits_for_family(family);
        }
        if let Some(prefix) = strip_anthropic_date_suffix(model)
            && let Some(family) = anthropic_alias_family(prefix)
        {
            return limits_for_family(family);
        }
    }

    ModelLimits::default()
}

/// Strips a single `:batch` suffix and an `openai/`/`anthropic/` namespace
/// prefix, rejecting a doubled `:batch`, an empty id, or a remaining `/`.
fn unwrap_model_id(model: &str) -> Option<(String, ModelNamespace)> {
    let trimmed = model.strip_suffix(":batch").unwrap_or(model);
    if trimmed.contains(":batch") {
        return None;
    }

    let (rest, namespace) = if let Some(rest) = trimmed.strip_prefix("openai/") {
        (rest, ModelNamespace::OpenAI)
    } else if let Some(rest) = trimmed.strip_prefix("anthropic/") {
        (rest, ModelNamespace::Anthropic)
    } else {
        (trimmed, ModelNamespace::None)
    };
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some((rest.to_string(), namespace))
}

fn is_ascii_digits(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

/// Splits a trailing `-YYYY-MM-DD` suffix, like
/// `^(.*)-[0-9]{4}-[0-9]{2}-[0-9]{2}$`. Operates on bytes so it never panics
/// on a non-UTF8-boundary split of a (rejected-anyway) non-ASCII model id.
fn strip_openai_date_suffix(model: &str) -> Option<&str> {
    let split = model.len().checked_sub(11)?;
    if !model.is_char_boundary(split) {
        return None;
    }
    let suffix = &model.as_bytes()[split..];
    let ok = suffix[0] == b'-'
        && is_ascii_digits(&suffix[1..5])
        && suffix[5] == b'-'
        && is_ascii_digits(&suffix[6..8])
        && suffix[8] == b'-'
        && is_ascii_digits(&suffix[9..11]);
    ok.then(|| &model[..split])
}

/// Splits a trailing `-YYYYMMDD` suffix, like `^(.*)-[0-9]{8}$`.
fn strip_anthropic_date_suffix(model: &str) -> Option<&str> {
    let split = model.len().checked_sub(9)?;
    if !model.is_char_boundary(split) {
        return None;
    }
    let suffix = &model.as_bytes()[split..];
    let ok = suffix[0] == b'-' && is_ascii_digits(&suffix[1..9]);
    ok.then(|| &model[..split])
}

fn openai_alias_family(model: &str) -> Option<ModelFamily> {
    use ModelFamily::*;
    Some(match model {
        "gpt-5.6" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => GptLong,
        "gpt-5.5" | "gpt-5.5-pro" => GptLong,
        "gpt-5.4" | "gpt-5.4-pro" => GptLong,
        "gpt-5.4-mini" | "gpt-5.4-nano" => Gpt400K,
        "gpt-5.3-chat-latest" => GptChat,
        "gpt-5.3-codex" => Gpt400K,
        "gpt-5.3-codex-spark" => GptSpark,
        "gpt-5.2" | "gpt-5.2-pro" | "gpt-5.2-codex" => Gpt400K,
        "gpt-5.2-chat-latest" => GptChat,
        "gpt-5.1" | "gpt-5.1-codex" | "gpt-5.1-codex-mini" | "gpt-5.1-codex-max" => Gpt400K,
        "gpt-5.1-chat-latest" => GptChat,
        "gpt-5" | "gpt-5-pro" | "gpt-5-mini" | "gpt-5-nano" | "gpt-5-codex" => Gpt400K,
        "gpt-5-chat-latest" => GptChat,
        "gpt-4.1" | "gpt-4.1-mini" | "gpt-4.1-nano" => Gpt41,
        "gpt-4o" | "gpt-4o-mini" => Gpt4o,
        "o1" | "o1-pro" | "o3" | "o3-mini" | "o3-pro" | "o4-mini" => OSeries,
        _ => return None,
    })
}

fn anthropic_alias_family(model: &str) -> Option<ModelFamily> {
    use ModelFamily::*;
    Some(match model {
        "claude-fable-5" | "claude-opus-5" | "claude-sonnet-5" => ClaudeMillion128,
        "claude-opus-4-8" | "claude-opus-4.8" => ClaudeMillion128,
        "claude-opus-4-7" | "claude-opus-4.7" => ClaudeMillion128,
        "claude-opus-4-6" | "claude-opus-4.6" => ClaudeMillion128,
        "claude-opus-4-5" | "claude-opus-4.5" => Claude200K64,
        "claude-opus-4-1" | "claude-opus-4.1" | "claude-opus-4" => Claude200K32,
        "claude-sonnet-4-6" | "claude-sonnet-4.6" => ClaudeMillion128,
        "claude-sonnet-4-5" | "claude-sonnet-4.5" => ClaudeMillion64,
        "claude-sonnet-4" => Claude200K64,
        "claude-3-7-sonnet" | "claude-3.7-sonnet" => Claude200K64,
        "claude-3-5-sonnet" | "claude-3.5-sonnet" => Claude200K8192,
        "claude-haiku-4-5" | "claude-haiku-4.5" => Claude200K64,
        "claude-3-5-haiku" | "claude-3.5-haiku" => Claude200K8192,
        "claude-3-haiku" => Claude200K4096,
        _ => return None,
    })
}

fn exact_snapshot_family(model: &str) -> Option<ModelFamily> {
    use ModelFamily::*;
    Some(match model {
        "gpt-4o-2024-05-13" => Gpt4o4096,
        "gpt-4o-2024-08-06" | "gpt-4o-2024-11-20" | "gpt-4o-mini-2024-07-18" => Gpt4o,
        _ => return None,
    })
}

fn limits_for_family(family: ModelFamily) -> ModelLimits {
    use ModelFamily::*;
    match family {
        GptLong => known(
            1_050_000,
            922_000,
            272_000,
            128_000,
            OPENAI_MODEL_SOURCE_URL,
        ),
        Gpt400K => known(400_000, 272_000, 272_000, 128_000, OPENAI_MODEL_SOURCE_URL),
        GptChat => known(128_000, 128_000, 128_000, 16_384, OPENAI_MODEL_SOURCE_URL),
        GptSpark => known(128_000, 128_000, 128_000, 32_000, OPENAI_MODEL_SOURCE_URL),
        Gpt41 => known(
            1_047_576,
            1_047_576,
            1_047_576,
            32_768,
            OPENAI_MODEL_SOURCE_URL,
        ),
        Gpt4o => known(128_000, 128_000, 128_000, 16_384, OPENAI_MODEL_SOURCE_URL),
        Gpt4o4096 => known(128_000, 128_000, 128_000, 4_096, OPENAI_MODEL_SOURCE_URL),
        OSeries => known(200_000, 200_000, 200_000, 100_000, OPENAI_MODEL_SOURCE_URL),
        ClaudeMillion128 => known(
            1_000_000,
            1_000_000,
            1_000_000,
            128_000,
            ANTHROPIC_MODEL_SOURCE_URL,
        ),
        ClaudeMillion64 => known(
            1_000_000,
            1_000_000,
            1_000_000,
            64_000,
            ANTHROPIC_MODEL_SOURCE_URL,
        ),
        Claude200K64 => known(
            200_000,
            200_000,
            200_000,
            64_000,
            ANTHROPIC_MODEL_SOURCE_URL,
        ),
        Claude200K32 => known(
            200_000,
            200_000,
            200_000,
            32_000,
            ANTHROPIC_MODEL_SOURCE_URL,
        ),
        Claude200K8192 => known(200_000, 200_000, 200_000, 8_192, ANTHROPIC_MODEL_SOURCE_URL),
        Claude200K4096 => known(200_000, 200_000, 200_000, 4_096, ANTHROPIC_MODEL_SOURCE_URL),
    }
}

fn known(
    context: i64,
    hard_input: i64,
    working: i64,
    max_output: i64,
    source_url: &str,
) -> ModelLimits {
    ModelLimits {
        known: true,
        context_window: context,
        hard_input_window: hard_input,
        max_output_tokens: max_output,
        working_window: working,
        source_url: source_url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        model: &'static str,
        known: bool,
        context: i64,
        hard: i64,
        working: i64,
        output: i64,
    }

    fn known_case(model: &'static str, context: i64, hard: i64, working: i64, output: i64) -> Case {
        Case {
            model,
            known: true,
            context,
            hard,
            working,
            output,
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn every_baseline_alias_is_known() {
        let cases: &[Case] = &[
            known_case("gpt-5.6", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.6-sol", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.6-terra", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.6-luna", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.5", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.5-pro", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.4", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.4-pro", 1_050_000, 922_000, 272_000, 128_000),
            known_case("gpt-5.4-mini", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.4-nano", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.3-codex", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.3-codex-spark", 128_000, 128_000, 128_000, 32_000),
            known_case("gpt-5.2", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.2-pro", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.2-codex", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.1", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.1-codex", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.1-codex-mini", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.1-codex-max", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5-pro", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5-mini", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5-nano", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5-codex", 400_000, 272_000, 272_000, 128_000),
            known_case("gpt-5.3-chat-latest", 128_000, 128_000, 128_000, 16_384),
            known_case("gpt-5.2-chat-latest", 128_000, 128_000, 128_000, 16_384),
            known_case("gpt-5.1-chat-latest", 128_000, 128_000, 128_000, 16_384),
            known_case("gpt-5-chat-latest", 128_000, 128_000, 128_000, 16_384),
            known_case("gpt-4.1", 1_047_576, 1_047_576, 1_047_576, 32_768),
            known_case("gpt-4.1-mini", 1_047_576, 1_047_576, 1_047_576, 32_768),
            known_case("gpt-4.1-nano", 1_047_576, 1_047_576, 1_047_576, 32_768),
            known_case("gpt-4o", 128_000, 128_000, 128_000, 16_384),
            known_case("gpt-4o-mini", 128_000, 128_000, 128_000, 16_384),
            known_case("o1", 200_000, 200_000, 200_000, 100_000),
            known_case("o1-pro", 200_000, 200_000, 200_000, 100_000),
            known_case("o3", 200_000, 200_000, 200_000, 100_000),
            known_case("o3-mini", 200_000, 200_000, 200_000, 100_000),
            known_case("o3-pro", 200_000, 200_000, 200_000, 100_000),
            known_case("o4-mini", 200_000, 200_000, 200_000, 100_000),
            known_case("claude-fable-5", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-opus-5", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-sonnet-5", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-opus-4-8", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-opus-4.8", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-opus-4-7", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-opus-4.7", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-opus-4-6", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case("claude-opus-4.6", 1_000_000, 1_000_000, 1_000_000, 128_000),
            known_case(
                "claude-sonnet-4-6",
                1_000_000,
                1_000_000,
                1_000_000,
                128_000,
            ),
            known_case(
                "claude-sonnet-4.6",
                1_000_000,
                1_000_000,
                1_000_000,
                128_000,
            ),
            known_case("claude-sonnet-4-5", 1_000_000, 1_000_000, 1_000_000, 64_000),
            known_case("claude-sonnet-4.5", 1_000_000, 1_000_000, 1_000_000, 64_000),
            known_case("claude-opus-4-5", 200_000, 200_000, 200_000, 64_000),
            known_case("claude-opus-4.5", 200_000, 200_000, 200_000, 64_000),
            known_case("claude-haiku-4-5", 200_000, 200_000, 200_000, 64_000),
            known_case("claude-haiku-4.5", 200_000, 200_000, 200_000, 64_000),
            known_case("claude-opus-4-1", 200_000, 200_000, 200_000, 32_000),
            known_case("claude-opus-4.1", 200_000, 200_000, 200_000, 32_000),
            known_case("claude-opus-4", 200_000, 200_000, 200_000, 32_000),
            known_case("claude-sonnet-4", 200_000, 200_000, 200_000, 64_000),
            known_case("claude-3-7-sonnet", 200_000, 200_000, 200_000, 64_000),
            known_case("claude-3.7-sonnet", 200_000, 200_000, 200_000, 64_000),
            known_case("claude-3-5-sonnet", 200_000, 200_000, 200_000, 8_192),
            known_case("claude-3.5-sonnet", 200_000, 200_000, 200_000, 8_192),
            known_case("claude-3-5-haiku", 200_000, 200_000, 200_000, 8_192),
            known_case("claude-3.5-haiku", 200_000, 200_000, 200_000, 8_192),
            known_case("claude-3-haiku", 200_000, 200_000, 200_000, 4_096),
        ];
        for case in cases {
            let got = resolve_model_limits(case.model);
            assert_eq!(got.known, case.known, "{}", case.model);
            assert_eq!(got.context_window, case.context, "{}", case.model);
            assert_eq!(got.hard_input_window, case.hard, "{}", case.model);
            assert_eq!(got.working_window, case.working, "{}", case.model);
            assert_eq!(got.max_output_tokens, case.output, "{}", case.model);
            assert!(got.source_url.starts_with("https://"), "{}", case.model);
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn uses_exact_aliases_and_wrappers() {
        let known_cases: &[Case] = &[
            known_case("gpt-5.6-sol", 1_050_000, 922_000, 272_000, 128_000),
            known_case(
                "openai/gpt-5.6-sol:batch",
                1_050_000,
                922_000,
                272_000,
                128_000,
            ),
            known_case("gpt-5.3-codex-spark", 128_000, 128_000, 128_000, 32_000),
            known_case("gpt-4o-2024-05-13", 128_000, 128_000, 128_000, 4_096),
            known_case("gpt-4o-2024-08-06", 128_000, 128_000, 128_000, 16_384),
            known_case("gpt-4o-2024-11-20", 128_000, 128_000, 128_000, 16_384),
            known_case("gpt-4o-mini-2024-07-18", 128_000, 128_000, 128_000, 16_384),
            known_case(
                "anthropic/claude-sonnet-4.5",
                1_000_000,
                1_000_000,
                1_000_000,
                64_000,
            ),
            known_case(
                "claude-sonnet-4-5-20250929",
                1_000_000,
                1_000_000,
                1_000_000,
                64_000,
            ),
            known_case(
                "gpt-5.6-sol-2026-01-02",
                1_050_000,
                922_000,
                272_000,
                128_000,
            ),
            known_case(
                "openai/gpt-5.6-sol-2026-01-02:batch",
                1_050_000,
                922_000,
                272_000,
                128_000,
            ),
            known_case(
                "claude-opus-4.8-20261001:batch",
                1_000_000,
                1_000_000,
                1_000_000,
                128_000,
            ),
        ];
        for case in known_cases {
            let got = resolve_model_limits(case.model);
            assert_eq!(got.known, case.known, "{}", case.model);
            assert_eq!(got.context_window, case.context, "{}", case.model);
            assert_eq!(got.hard_input_window, case.hard, "{}", case.model);
            assert_eq!(got.working_window, case.working, "{}", case.model);
            assert_eq!(got.max_output_tokens, case.output, "{}", case.model);
        }

        for model in [
            "OPENAI/gpt-5.6-sol",
            "azure-gpt-5.6-sol",
            "my-gpt-model",
            "anthropic/gpt-5.6-sol",
            "openai/claude-sonnet-4-5",
            "openai/openai/gpt-5.6-sol",
            "gpt-5.6-sol:batch:batch",
            "gpt-5.6-sol:batch-extra",
            "gpt-5.6-sol-2026-01-02-extra",
            "gpt-5.6-sol-preview-2026-01-02",
            "claude-sonnet-4-5-20250929-extra",
            "claude-sonnet-4-5-preview-20250929",
        ] {
            let got = resolve_model_limits(model);
            assert!(!got.known, "{model}");
            assert_eq!(got.context_window, 0, "{model}");
            assert_eq!(got.source_url, "", "{model}");
        }
    }
}
