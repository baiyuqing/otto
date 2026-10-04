//! Scheduled deliveries: text or prompt items that are delivered to the
//! origin that created them, now or repeatedly.
//!
//! Design: `docs/specs/2026-10-04-scheduled-deliveries.md`. This module owns
//! the origin model, the time rules ([`time`]), the cross-process store
//! ([`store`]), the three model tools ([`tools`]) and the worker that runs
//! prompts and delivers results ([`worker`]). Callers wire it in: the app
//! layer sets [`tools::TurnOrigin`], the composition roots build a
//! [`worker::Worker`] with a [`worker::PromptRunner`] and a [`worker::Sink`].

pub mod store;
pub mod time;
pub mod tools;
pub mod worker;

pub use store::{
    CreateError, Delivery, Error, Item, NewItem, Result, Store, delivery_text, parse_label,
    run_text,
};

/// Platforms an otto-connect chat can belong to.
const CHAT_PLATFORMS: [&str; 2] = ["telegram", "feishu"];
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

    /// `platform` must be `telegram` or `feishu`; `chat_id` is 1..=128
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

    pub fn is_chat(&self) -> bool {
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
            Some("telegram") => "this Telegram chat".into(),
            Some(_) => "this Feishu chat".into(),
            None if self.0 == "web" => "the otto web UI".into(),
            None => "any running otto terminal (TUI or REPL)".into(),
        }
    }
}

/// Which origins a process runs or delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Any,
    Terminal,
    Web,
    /// Chat platforms, as in [`Origin::chat_platform`].
    Chats(Vec<String>),
}

impl Scope {
    pub fn matches(&self, origin: &Origin) -> bool {
        match self {
            Self::Any => true,
            Self::Terminal => origin.as_str() == "terminal",
            Self::Web => origin.as_str() == "web",
            Self::Chats(platforms) => origin
                .chat_platform()
                .is_some_and(|platform| platforms.iter().any(|p| p == platform)),
        }
    }
}

macro_rules! stored_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

stored_enum!(
    /// What an item delivers: its body, or the reply of a run of its body.
    Kind { Text => "text", Prompt => "prompt" }
);
stored_enum!(
    /// Stored as the empty string for a one-shot item.
    Repeat { Daily => "daily", Weekdays => "weekdays" }
);
stored_enum!(
    State {
        Waiting => "waiting",
        Ready => "ready",
        Done => "done",
        Failed => "failed",
        Cancelled => "cancelled",
    }
);

impl State {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Waiting | Self::Ready)
    }
}

/// The result of one scheduled prompt run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Reply(String),
    /// Failed before any tool call started; the run may be repeated.
    FailedBeforeTools(String),
    /// Failed after a tool call started, or for a reason a retry does not fix.
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_round_trips_and_rejects_other_forms() {
        for text in [
            "terminal",
            "web",
            "chat:telegram:-100123",
            "chat:feishu:oc_abc",
        ] {
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
        let origin = Origin::parse("chat:feishu:a:b").unwrap();
        assert_eq!(origin.chat_platform(), Some("feishu"));
        assert!(origin.is_chat());
        assert_eq!(Origin::web().chat_platform(), None);
    }

    #[test]
    fn scope_matches_by_origin_kind_and_platform() {
        let tg = Origin::chat("telegram", "1").unwrap();
        let fs = Origin::chat("feishu", "1").unwrap();
        assert!(Scope::Any.matches(&tg) && Scope::Any.matches(&Origin::web()));
        assert!(Scope::Terminal.matches(&Origin::terminal()));
        assert!(!Scope::Terminal.matches(&Origin::web()));
        assert!(Scope::Web.matches(&Origin::web()));
        let chats = Scope::Chats(vec!["telegram".into()]);
        assert!(chats.matches(&tg));
        assert!(!chats.matches(&fs));
        assert!(!chats.matches(&Origin::terminal()));
    }
}
