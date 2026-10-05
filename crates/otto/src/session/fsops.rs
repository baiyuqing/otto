//! Session-specific keys and entry IDs over the shared native filesystem layer.

#![allow(unsafe_code)]

pub(crate) use crate::fsops::*;
use otto_core::session::PiError;
use std::io;
use std::path::Path;

/// Four random bytes, hex encoded, avoiding any id in `seen`. A fresh Pi entry
/// id, including the 1024-attempt ceiling.
pub fn new_pi_entry_id(seen: &std::collections::HashSet<String>) -> Result<String, PiError> {
    for _ in 0..1024 {
        let mut buffer = [0u8; 4];
        // SAFETY: buffer is 4 writable bytes and 4 is well under the 256-byte
        // getentropy limit.
        if unsafe { libc::getentropy(buffer.as_mut_ptr().cast(), buffer.len()) } < 0 {
            return Err(PiError::other(io::Error::last_os_error()));
        }
        let id = buffer
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !seen.contains(&id) {
            return Ok(id);
        }
    }
    Err(PiError::other(
        "could not generate a collision-free session entry id",
    ))
}

/// The marker's stable session key, or the legacy digest when no marker exists.
pub fn workspace_key(workspace: &Path) -> Result<String, PiError> {
    let canonical = canonical_workspace(workspace)?;
    crate::workspace::session_key(&canonical)
}
