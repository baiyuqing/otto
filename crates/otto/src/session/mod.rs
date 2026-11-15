//! Native session store: the `SessionRepository` contracts, the JSONL
//! journal [`Store`] with its listing, fork, archive and delete operations,
//! and the writer lease in [`lease`]. The journal files are read and written
//! through the `fsops::files` contract.

mod fork;
pub(crate) mod fsops;
pub mod lease;
mod legacy;
mod list;
mod list_for_delete;
pub mod origin;
mod permanent_delete;
mod posix_repository;
pub(crate) mod prepared;
mod repository;
pub(crate) mod store;

#[cfg(test)]
mod tests;

pub use fork::Forked;
pub use fsops::workspace_key;
pub use legacy::archive;
pub use list::{MAX_LIST_SESSIONS, inspect, list, session_directory};
pub(crate) use otto_platform::fsops::clean_go_path;
pub use posix_repository::PosixSessionRepository;
pub(crate) use prepared::ArchiveResult;
pub use prepared::Prepared;
pub(crate) use repository::WriterOwnership;
pub use repository::{DeleteCandidate, PreparedSession, SessionHandle, SessionRepository};
pub(crate) use store::UnansweredCall;
pub(crate) use store::unanswered_calls_from;
pub use store::{Recovery, RecoveryCause, Store};

/// A session id is 32 lowercase hexadecimal characters. Checked before an id
/// from a client becomes `<session directory>/<id>.jsonl`, so it cannot name
/// another path.
pub fn is_session_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
