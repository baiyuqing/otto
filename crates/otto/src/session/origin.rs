//! Where a user turn came from.
//!
//! [`Origin`] is the wire and storage form that scheduled items, approvals and
//! memory records carry; [`TurnOrigin`] holds the origin of the running turn.

use std::sync::Mutex;

/// Platforms an otto-connect chat can belong to.
const CHAT_PLATFORMS: [&str; 1] = ["telegram"];
const MAX_CHAT_ID_CHARS: usize = 128;

/// Where a user turn came from, and where a scheduled item is delivered.
/// Wire/storage form: `terminal`, `web`, `chat:<platform>:<chat id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin(String);

impl Origin {
    pub fn terminal() -> Self {
        Self("terminal".into())
    }

    pub fn web() -> Self {
        Self("web".into())
    }

    /// `platform` must be `telegram`; `chat_id` is 1..=128
    /// characters with no whitespace or control characters.
    pub fn chat(platform: &str, chat_id: &str) -> std::result::Result<Self, String> {
        if !CHAT_PLATFORMS.contains(&platform) {
            return Err(format!(
                "chat platform must be one of {}, got {platform:?}",
                CHAT_PLATFORMS.join(", ")
            ));
        }
        let length = chat_id.chars().count();
        if length == 0 || length > MAX_CHAT_ID_CHARS {
            return Err(format!(
                "chat id must be 1 to {MAX_CHAT_ID_CHARS} characters, got {length}"
            ));
        }
        if chat_id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("chat id must not contain whitespace or control characters".into());
        }
        Ok(Self(format!("chat:{platform}:{chat_id}")))
    }

    /// Parses the storage/wire form.
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value {
            "terminal" => Ok(Self::terminal()),
            "web" => Ok(Self::web()),
            _ => {
                let rest = value.strip_prefix("chat:").ok_or_else(|| {
                    format!(
                        "origin must be \"terminal\", \"web\" or \"chat:<platform>:<chat id>\", got {value:?}"
                    )
                })?;
                let (platform, chat_id) = rest.split_once(':').ok_or_else(|| {
                    format!("chat origin must be \"chat:<platform>:<chat id>\", got {value:?}")
                })?;
                Self::chat(platform, chat_id)
            }
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `Some("telegram")` for a chat origin.
    pub fn chat_platform(&self) -> Option<&str> {
        self.0.strip_prefix("chat:")?.split(':').next()
    }

    pub(crate) fn is_chat(&self) -> bool {
        self.0.starts_with("chat:")
    }

    /// Used in the header of a scheduled run.
    pub fn describe(&self) -> String {
        match self.chat_platform() {
            Some(platform) => format!("a {platform} chat"),
            None if self.0 == "web" => "the web UI".into(),
            None => "the terminal".into(),
        }
    }

    /// Used in the `schedule` tool result.
    pub fn target(&self) -> String {
        match self.chat_platform() {
            Some(_) => "this Telegram chat".into(),
            None if self.0 == "web" => "the otto web UI".into(),
            None => "any running otto terminal (TUI or REPL)".into(),
        }
    }
}

/// The origin of the current user turn of one runner. Set by the app layer at
/// the start of each user turn; a wake turn keeps the last value.
#[derive(Default)]
pub struct TurnOrigin(Mutex<Option<Origin>>);

impl TurnOrigin {
    pub fn set(&self, origin: Option<Origin>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = origin;
    }

    pub fn get(&self) -> Option<Origin> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_round_trips_and_rejects_other_forms() {
        for text in ["terminal", "web", "chat:telegram:-100123"] {
            assert_eq!(Origin::parse(text).unwrap().as_str(), text);
        }
        for bad in [
            "",
            "Terminal",
            "chat",
            "chat:telegram",
            "chat:telegram:",
            "chat:slack:1",
            "chat:telegram:a b",
            "chat:telegram:a\nb",
            &format!("chat:telegram:{}", "x".repeat(129)),
        ] {
            assert!(Origin::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Origin::chat("telegram", &"x".repeat(128)).is_ok());
        // The chat id may itself contain colons: only the first splits.
        let origin = Origin::parse("chat:telegram:a:b").unwrap();
        assert_eq!(origin.chat_platform(), Some("telegram"));
        assert!(origin.is_chat());
        assert_eq!(Origin::web().chat_platform(), None);
    }
}
