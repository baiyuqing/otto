//! Stable native session façade; the POSIX implementation lives under
//! `storage::backends::posix::session`.

pub(crate) mod fsops;
mod legacy;
mod repository;

pub use crate::fsops::clean_go_path;
pub(crate) use crate::storage::backends::posix::session::unanswered_calls_from;
pub use crate::storage::backends::posix::session::{
    ArchiveResult, Forked, MAX_AUTOMATIC_RESUMES, MAX_LIST_SESSIONS, Prepared, Recovery,
    RecoveryCause, Store, UnansweredCall, inspect, list, session_directory,
};
pub(crate) use fsops::workspace_key;
pub use legacy::archive;
pub use repository::{PreparedSession, SessionHandle, SessionRepository, WriterOwnership};

/// A session id is 32 lowercase hexadecimal characters. Checked before an id
/// from a client becomes `<session directory>/<id>.jsonl`, so it cannot name
/// another path.
pub fn is_session_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
