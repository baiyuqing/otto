//! Scheduled deliveries: text or prompt items that are delivered to the
//! origin that created them, now or repeatedly.
//!
//! This module owns
//! the origin scope (the origin value is [`otto_session::origin::Origin`]),
//! the time rules ([`time`]), the cross-process store ([`store`]), the three
//! model tools ([`tools`]), the timer tools ([`remind`]) and the worker that
//! runs prompts and delivers results ([`worker`]). Callers wire it in: the app
//! layer sets [`otto_session::origin::TurnOrigin`], the composition roots
//! build a [`worker::Worker`] with a [`worker::PromptRunner`] and a
//! [`worker::Sink`].

pub mod remind;
pub mod sqlite;
pub mod store;
pub mod time;
pub mod tools;
pub mod worker;

pub use crate::schedule::sqlite::SqliteScheduleStore as Store;
use otto_session::origin::Origin;
pub(crate) use store::CreateError;
#[cfg(test)]
pub(crate) use store::run_text;
pub use store::{
    Delivery, Error, Item, NewItem, Result, ScheduleStore, WorkspaceAdmit, delivery_text,
    parse_label,
};

/// Which origins a process runs or delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Any,
    Terminal,
    Web,
    /// Explicit admitted destinations.
    Origins(Vec<Origin>),
    /// Resolved against live client registrations by the store before claiming.
    DeliveryClient(String),
}

impl Scope {
    pub fn matches(&self, origin: &Origin) -> bool {
        match self {
            Self::Any => true,
            Self::Terminal => origin.as_str() == "terminal",
            Self::Web => origin.as_str() == "web",
            Self::Origins(origins) => origins.contains(origin),
            Self::DeliveryClient(_) => false,
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
    fn scope_matches_by_origin_kind_and_destination() {
        let tg = Origin::chat("telegram", "1").unwrap();
        assert!(Scope::Any.matches(&tg) && Scope::Any.matches(&Origin::web()));
        assert!(Scope::Terminal.matches(&Origin::terminal()));
        assert!(!Scope::Terminal.matches(&Origin::web()));
        assert!(Scope::Web.matches(&Origin::web()));
        let chats = Scope::Origins(vec![tg.clone()]);
        assert!(chats.matches(&tg));
        assert!(!Scope::Origins(Vec::new()).matches(&tg));
        assert!(!chats.matches(&Origin::terminal()));
    }
}
