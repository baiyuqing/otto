//! Per-session artifact directories under `.otto/tool-output` and
//! `.otto/reports`. The directory separates names between sessions; it is not
//! access control, since every session can open the shared parent directory.
use std::io::{self, ErrorKind};

use nix::sys::stat::Mode;

use otto_platform::fsops::root::Root;

/// Accepts a session id only when it is exactly one normal path component.
pub(crate) fn session_directory_name(session_id: &str) -> io::Result<&str> {
    if session_id.is_empty()
        || session_id == "."
        || session_id == ".."
        || session_id.contains('/')
        || session_id.contains('\0')
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "session id is not a single path component",
        ));
    }
    Ok(session_id)
}

/// Opens (creating when missing) `<parent>/<session id>` mode 0700 through the
/// pinned `parent` handle, without following a symlink.
pub(crate) fn create_session_directory(parent: &Root, session_id: &str) -> io::Result<Root> {
    parent.create_child_directory(session_directory_name(session_id)?, Mode::S_IRWXU)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_single_normal_component_is_accepted() {
        for id in ["", ".", "..", "a/b", "/abs", "a\0b"] {
            assert!(session_directory_name(id).is_err(), "{id:?}");
        }
        assert_eq!(session_directory_name("abc-123").unwrap(), "abc-123");
    }
}
