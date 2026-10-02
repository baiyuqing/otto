//! The file-backed session store.
//!
//! `otto-core::session` owns the wire format and every pure rule; this module
//! owns the files: creating them with the right modes, appending durably,
//! listing them, archiving them, and refusing to follow a symlink on the way.
//!
//! Ownership: a [`Store`] owns one open session file. Concurrency: every store
//! method takes the store's own mutex, so a `Store` is `Send + Sync`. Errors:
//! everything is an `otto_core::session::PiError`.

pub(crate) mod fsops;
mod list;
mod prepared;
mod store;

#[cfg(test)]
mod tests;

pub use fsops::clean_go_path;
pub(crate) use fsops::workspace_key;
pub use list::{MAX_LIST_SESSIONS, inspect, list, session_directory};
pub use prepared::{ArchiveResult, Prepared, archive};
pub(crate) use store::unanswered_calls_from;
pub use store::{Store, Takeover, UnansweredCall};

/// A session id is 32 lowercase hexadecimal characters. Checked before an id
/// from a client becomes `<session directory>/<id>.jsonl`, so it cannot name
/// another path.
pub fn is_session_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
