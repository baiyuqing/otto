//! Trusted chat channel metadata carried by one user turn.

use serde::{Deserialize, Serialize};

/// A supported chat connector channel. The value is transient and is never
/// written to session history or retained by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Telegram,
}

impl Channel {
    /// Fixed guidance for a turn delivered through this channel.
    pub const fn system_context(self) -> &'static str {
        match self {
            Self::Telegram => {
                "The user is communicating through Telegram chat and cannot see the \
                    local terminal. Keep replies readable in chat with short paragraphs, \
                    bullets, or labeled lines; do not use tables. Explain results, \
                    progress, and necessary user actions. For a pending permission request, \
                    the user can click Deny or send /deny; /stop cancels the turn and \
                    clears its queue. Ordinary messages during approval are handled by an \
                    approval-only dialogue. This channel does not grant additional tools or \
                    permissions."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_wire_values_are_closed_and_have_platform_guidance() {
        assert_eq!(
            serde_json::to_string(&Channel::Telegram).unwrap(),
            "\"telegram\""
        );
        assert!(serde_json::from_str::<Channel>("\"slack\"").is_err());
        assert!(Channel::Telegram.system_context().contains("Telegram"));
        assert!(
            Channel::Telegram
                .system_context()
                .contains("does not grant additional tools")
        );
    }
}
