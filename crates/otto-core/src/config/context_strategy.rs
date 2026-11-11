//! Selection of the provider context layout.

use serde::{Deserialize, Serialize};

/// How ordinary turns project session history into provider input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextStrategy {
    #[default]
    Compaction,
    ViewTree,
}

/// The optional `[agent.context]` table preserves field presence during merging.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    pub strategy: Option<ContextStrategy>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        edit,
        layered::{Layer, ManagedPolicy, Merged},
        resolve::{self, Overrides, SessionDefaults},
    };

    #[test]
    fn strategy_defaults_resolves_and_rejects_invalid_values() {
        for (value, expected) in [
            (None, ContextStrategy::Compaction),
            (Some("compaction"), ContextStrategy::Compaction),
            (Some("view-tree"), ContextStrategy::ViewTree),
        ] {
            let text = format!(
                "default_model='chatgpt/test'\n[providers.chatgpt]\ntype='chatgpt'\n[providers.chatgpt.models.test]\n{}",
                value.map_or(String::new(), |value| format!(
                    "[agent.context]\nstrategy='{value}'"
                ))
            );
            let file = crate::config::parse(&text).unwrap();
            let runtime = resolve::resolve_without_credentials(
                &file,
                &Default::default(),
                &SessionDefaults::default(),
                &Overrides::default(),
            )
            .unwrap();
            assert_eq!(runtime.context_strategy, expected);
        }
        assert!(crate::config::parse("[agent.context]\nstrategy='unknown'").is_err());
        assert!(crate::config::parse("[agent.context]\nunrecognized=true").is_err());
        assert_eq!(
            crate::config::parse("").unwrap().agent.context.strategy,
            None
        );
    }

    #[test]
    fn project_selection_and_managed_override_preserve_provenance() {
        let mut merged = Merged::default();
        merged
            .add(
                "[agent.context]\nstrategy='compaction'",
                "user",
                Layer::User,
                false,
            )
            .unwrap();
        merged
            .add(
                "[agent.context]\nstrategy='view-tree'",
                "project",
                Layer::Shared,
                false,
            )
            .unwrap();
        assert_eq!(
            merged.file().unwrap().agent.context.strategy,
            Some(ContextStrategy::ViewTree)
        );
        assert_eq!(merged.origins["agent.context.strategy"].source, "project");
        let policy: ManagedPolicy =
            toml::from_str("[force.agent.context]\nstrategy='compaction'").unwrap();
        policy.apply(&mut merged, "managed").unwrap();
        assert_eq!(
            merged.file().unwrap().agent.context.strategy,
            Some(ContextStrategy::Compaction)
        );
        assert_eq!(merged.origins["agent.context.strategy"].source, "managed");
    }

    #[test]
    fn editing_strategy_preserves_unrelated_settings_and_comments() {
        let text = "# context\n[agent.context]\nstrategy = 'compaction' # selected\n\n[agent.compaction]\nauto = false\n";
        let edited =
            edit::set_value(text, &["agent", "context"], "strategy", Some("'view-tree'")).unwrap();
        assert_eq!(edited, text.replace("'compaction'", "'view-tree'"));
    }
}
