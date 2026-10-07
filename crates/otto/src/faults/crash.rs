//! Process death at a named operation, for [`super::Fault::Exit`] and its
//! variants.

/// The exit status of a process ended by an injected [`super::Fault::Exit`].
pub const CRASH_EXIT: i32 = 86;

/// End the process at once with [`CRASH_EXIT`]. No destructor, buffered
/// writer, lock release or atexit handler runs, so this models `SIGKILL`,
/// not power loss: data already written to the kernel stays.
pub fn exit() -> ! {
    // SAFETY: `_exit` takes no pointers and never returns.
    #[allow(unsafe_code)]
    unsafe {
        libc::_exit(CRASH_EXIT)
    }
}
