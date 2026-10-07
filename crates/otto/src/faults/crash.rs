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

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Environment variable that marks a re-executed test binary as the child and
/// names the directory shared with the parent.
const DIR_ENV: &str = "OTTO_FAULT_CRASH_DIR";

/// The child must end within this bound; a longer run is killed and fails the
/// parent.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// The part a test body plays in one [`run`].
pub enum Role<'a> {
    /// In the re-executed process. The directory is shared with the parent.
    /// The body must end the process through an injected `Exit` fault.
    Child(&'a Path),
    /// In the original test process, after the child ended.
    Parent(&'a Path, ExitStatus),
}

/// The libtest name of the test that contains the closure `marker`, for
/// [`run`]: `run(name_of(&|| {}), ...)`. The closure type's path is the
/// crate, the module path, the test function and `{{closure}}`; libtest names
/// a test without its crate.
pub fn name_of<F: Fn()>(marker: &F) -> String {
    let path = std::any::type_name_of_val(marker);
    let path = path.strip_suffix("::{{closure}}").unwrap_or(path);
    path.split_once("::")
        .map_or(path, |(_, test)| test)
        .to_owned()
}

/// Runs `body` in a child process that ends in an injected crash, then in the
/// parent. The parent re-executes the current test binary for the test
/// `name` (see [`name_of`]) with `OTTO_FAULT_CRASH_DIR` set to a fresh
/// directory, waits for it and calls `body` with its exit status. A child
/// whose body returns fails with a panic, so the parent sees a status other
/// than [`CRASH_EXIT`].
pub fn run(name: String, body: impl FnOnce(Role<'_>)) {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        body(Role::Child(Path::new(&dir)));
        panic!("crash child body returned: the injected fault did not end the process");
    }
    let dir = tempfile::tempdir().expect("crash directory");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(DIR_ENV, dir.path())
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn crash child");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for crash child") {
            break status;
        }
        if started.elapsed() > CHILD_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("crash child {name} did not end within {CHILD_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    body(Role::Parent(dir.path(), status));
}
