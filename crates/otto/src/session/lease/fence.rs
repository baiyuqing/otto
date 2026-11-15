//! The keeper action that stops this process when its writer lease is lost.

#![allow(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use super::{FENCED_EXIT_STATUS, Keeper};
use otto_platform::child_fence::Children;

/// The runtime composition root supplies this keeper to the POSIX session
/// repository. The local session constructors use the same process-fencing
/// action.
pub(crate) fn new_keeper() -> Arc<Keeper> {
    Keeper::new(Box::new(fence_current_process), Duration::from_secs(1))
}

fn fence_current_process() {
    Children::global().kill_all();
    // SAFETY: losing a writer lease requires stopping this process without
    // running destructors that might write session state.
    unsafe { libc::_exit(FENCED_EXIT_STATUS) }
}
