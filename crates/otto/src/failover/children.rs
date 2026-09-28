//! Registry of process ids started by this process, killed together when a
//! lease's watchdog fences this process.
//!
//! Ownership: one process-wide [`Children`], reached via [`Children::global`].
//! Concurrency: a `Mutex<State>` guards the registered set; `register` and
//! `kill_all` each take it once and release it before signaling. Errors:
//! `kill_all` is best-effort and never fails; a pid that has already exited
//! yields `ESRCH` from the kill syscall, which is ignored.

use std::sync::{Arc, Mutex, OnceLock};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

#[derive(Debug, Clone, Copy)]
struct Entry {
    id: u64,
    pid: i32,
    process_group: bool,
}

#[derive(Default)]
struct State {
    next_id: u64,
    entries: Vec<Entry>,
}

/// Process-wide registry of started children, consulted by a lease's
/// watchdog when it fences this process.
pub struct Children {
    state: Mutex<State>,
}

impl std::fmt::Debug for Children {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Children").finish_non_exhaustive()
    }
}

impl Children {
    /// A standalone registry, isolated from [`Children::global`]. A test
    /// `Keeper` uses its own instance so `kill_all` cannot reach a process
    /// group a concurrently running test registered on the global one.
    pub(crate) fn new() -> Self {
        Children {
            state: Mutex::new(State::default()),
        }
    }

    /// The process-wide instance.
    pub fn global() -> &'static Arc<Children> {
        static INSTANCE: OnceLock<Arc<Children>> = OnceLock::new();
        INSTANCE.get_or_init(|| Arc::new(Children::new()))
    }

    /// Registers `pid` for [`Children::kill_all`]. When `process_group` is
    /// true, `pid` is also the id of the process group it leads (the caller
    /// must have started it with its own process group), and `kill_all`
    /// signals the group instead of the single process. Returns a guard
    /// that unregisters the entry on drop.
    pub fn register(children: &Arc<Self>, pid: i32, process_group: bool) -> Registration {
        let mut state = children.state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        state.entries.push(Entry {
            id,
            pid,
            process_group,
        });
        Registration {
            children: Arc::clone(children),
            id,
        }
    }

    /// Sends `SIGKILL` to every registered pid or process group. Best-effort:
    /// an already-exited pid is skipped.
    pub fn kill_all(&self) {
        let entries = self.state.lock().unwrap().entries.clone();
        for entry in entries {
            let target = if entry.process_group {
                -entry.pid
            } else {
                entry.pid
            };
            let _ = signal::kill(Pid::from_raw(target), Signal::SIGKILL);
        }
    }

    fn unregister(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        state.entries.retain(|entry| entry.id != id);
    }
}

/// Guard returned by [`Children::register`]. Dropping it removes the entry
/// so a later `kill_all` no longer signals it.
pub struct Registration {
    children: Arc<Children>,
    id: u64,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.children.unregister(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn kill_all_kills_a_registered_child() {
        let children = Arc::new(Children::new());
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        let _reg = Children::register(&children, pid, false);

        children.kill_all();

        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn dropping_registration_excludes_it_from_kill_all() {
        let children = Arc::new(Children::new());
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        {
            let _reg = Children::register(&children, pid, false);
        }

        children.kill_all();
        std::thread::sleep(Duration::from_millis(100));

        assert!(
            child.try_wait().unwrap().is_none(),
            "unregistered child was killed"
        );
        child.kill().unwrap();
        let _ = child.wait();
    }
}
