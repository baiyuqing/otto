//! Compatibility and process-fencing facade for POSIX session leases.

#![allow(unsafe_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::failover::children::Children;
use crate::storage::backends::posix::lease::{self, Keeper};

#[cfg(test)]
pub(crate) use crate::storage::backends::posix::lease::without_acquire_wait;
pub use crate::storage::backends::posix::lease::{
    AcquireError, Acquired, DEFAULT_LEASE_SECONDS, FENCED_EXIT_STATUS, Holder, Lease,
    create_lease_dir, is_lease_managed, lease_dir, local_hostname,
};

/// The runtime composition root supplies this keeper to the POSIX session
/// repository. Legacy session constructors use the same process-fencing
/// action through this compatibility constructor.
pub(crate) fn new_keeper() -> Arc<Keeper> {
    Keeper::new(Box::new(fence_current_process), Duration::from_secs(1))
}

fn fence_current_process() {
    Children::global().kill_all();
    // SAFETY: losing a writer lease requires stopping this process without
    // running destructors that might write session state.
    unsafe { libc::_exit(FENCED_EXIT_STATUS) }
}

impl Lease {
    /// Compatibility entry point. Runtime repositories should reuse their
    /// composed keeper through the backend-specific constructor.
    pub fn acquire(path: &Path) -> Result<(Arc<Lease>, Acquired), AcquireError> {
        let keeper = new_keeper();
        lease::Lease::acquire_with_keeper(path, &keeper)
    }
}
