//! Static picker entries, independent of endpoint trust and transport policy.

use super::{PROVIDER_CHATGPT, PROVIDER_CLAUDE, PROVIDER_DEEPSEEK};

pub(super) fn for_provider(provider: &str) -> &'static [&'static str] {
    match provider {
        PROVIDER_CHATGPT => &[
            "gpt-6-luna",
            "gpt-6.1-terra",
            "gpt-6.2-sol",
            "gpt-6.2-astra",
            "gpt-5.6-luna",
            "gpt-5.6-terra",
            "gpt-5.6-sol",
        ],
        PROVIDER_CLAUDE => &["claude-haiku-5-5", "claude-opus-4-8", "claude-sonnet-4-6"],
        PROVIDER_DEEPSEEK => &["deepseek-flash", "deepseek-v4-pro"],
        _ => &[],
    }
}
