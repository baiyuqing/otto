//! The file-backed session store.
//!
//! Port of `internal/session`. `otto-core::session` owns the wire format and
//! every pure rule; this module owns the files: creating them with the right
//! modes, appending durably, listing them, archiving them, and refusing to
//! follow a symlink on the way.
//!
//! Ownership: a [`Store`] owns one open session file. Concurrency: every
//! store method takes the store's own mutex, so a `Store` is `Send + Sync`.
//! Errors: everything is an `otto_core::session::PiError`.

mod fsops;
mod list;
mod prepared;
mod store;

#[cfg(test)]
mod tests;

pub use fsops::clean_go_path;
pub use list::{MAX_LIST_SESSIONS, inspect, list, session_directory};
pub use prepared::{ArchiveResult, Prepared, archive};
pub use store::Store;
