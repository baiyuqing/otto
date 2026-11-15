//! User/managed approval security policy. Project layers have no authority here.
use super::ConfigError;
use crate::approval::{ApprovalPolicy, ApprovalReviewer, ApprovalRule, ApprovalSettings};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConfig {
    pub policy: Option<ApprovalPolicy>,
    pub reviewer: Option<ApprovalReviewer>,
    /// Selector `<provider>/<model_id>` of the reviewer model.
    pub reviewer_model: Option<String>,
    #[serde(default)]
    pub rules: Vec<ApprovalRule>,
}

impl ApprovalConfig {
    pub fn settings(&self) -> ApprovalSettings {
        ApprovalSettings {
            policy: self.policy.unwrap_or_default(),
            reviewer: self.reviewer.unwrap_or_default(),
            revision: 1,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(selector) = &self.reviewer_model {
            super::ModelSelector::parse(selector)
                .map_err(|error| ConfigError::new(format!("approval.reviewer_model: {error}")))?;
        }
        for rule in &self.rules {
            rule.validate()
                .map_err(|reason| ConfigError::new(format!("approval.rules: {reason}")))?;
        }
        Ok(())
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        layered::{Layer, Merged},
        parse,
    };

    #[test]
    fn defaults_and_validation() {
        assert_eq!(
            parse("").unwrap().approval.settings(),
            ApprovalSettings::default()
        );
        let config = parse("[approval]\npolicy='never'\nreviewer='user'\nreviewer_model='p/review'\n[[approval.rules]]\ntool='bash'\ndecision='ask'").unwrap();
        assert!(config.approval.settings().is_automatic());
        assert_eq!(config.approval.rules.len(), 1);
        for text in [
            "[approval]\npolicy='auto'",
            "[approval]\nreviewer_model='review'",
            "[[approval.rules]]\ndecision='allow'",
            "[[approval.rules]]\ntool='bash'\nsource='sandbox'\ndecision='allow'",
            "[[approval.rules]]\ncommand_prefix=[]\ndecision='allow'",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn project_layers_cannot_change_security_even_when_trusted() {
        for trusted in [false, true] {
            for layer in [Layer::Local, Layer::Shared] {
                let mut merged = Merged::default();
                merged
                    .add(
                        "[approval]\npolicy='on_request'\nreviewer='user'",
                        "user",
                        Layer::User,
                        false,
                    )
                    .unwrap();
                merged.add("[approval]\npolicy='never'\nreviewer='bypass'\nreviewer_model='evil/x'\n[[approval.rules]]\nsource='sandbox'\ndecision='allow'", "project", layer, trusted).unwrap();
                let config = merged.file().unwrap();
                assert_eq!(config.approval.reviewer, Some(ApprovalReviewer::User));
                assert_eq!(config.approval.policy, Some(ApprovalPolicy::OnRequest));
                assert_eq!(config.approval.reviewer_model, None);
                assert!(config.approval.rules.is_empty());
                assert!(
                    merged
                        .diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.key == "approval")
                );
            }
        }
    }
}
