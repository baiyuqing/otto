//! Controller admission, replacement publication and shutdown transitions.

use super::{Admission, CLOSED, Controller, Current, PROMPT_ACTIVE, Runner};

enum Activity {
    Idle,
    WakeReserved(u64),
    Active(u64),
}

enum Lifecycle {
    Open(Activity),
    /// Only a running operation may defer disposal. An unused wake is released.
    CloseRequested {
        active: Option<u64>,
    },
    Disposing,
    Closed(Result<(), String>),
}

pub(super) enum ReplacementCommit {
    Installed(Option<Current>),
    Rejected(Current),
}

pub(super) struct State {
    pub(super) current: Option<Current>,
    lifecycle: Lifecycle,
    generation: u64,
}

impl State {
    pub(super) fn new(current: Current) -> Self {
        Self {
            current: Some(current),
            lifecycle: Lifecycle::Open(Activity::Idle),
            generation: 0,
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        !matches!(self.lifecycle, Lifecycle::Open(_))
    }

    pub(super) fn is_busy(&self) -> bool {
        matches!(
            self.lifecycle,
            Lifecycle::Open(Activity::WakeReserved(_) | Activity::Active(_))
                | Lifecycle::CloseRequested { active: Some(_) }
        )
    }

    fn admit(&mut self) -> Result<u64, String> {
        match self.lifecycle {
            Lifecycle::Open(Activity::Idle) => {
                self.generation += 1;
                self.lifecycle = Lifecycle::Open(Activity::Active(self.generation));
                Ok(self.generation)
            }
            Lifecycle::Open(_) => Err(PROMPT_ACTIVE.into()),
            _ => Err(CLOSED.into()),
        }
    }

    pub(super) fn reserve_wake(&mut self) -> Result<u64, String> {
        let generation = self.admit()?;
        self.lifecycle = Lifecycle::Open(Activity::WakeReserved(generation));
        Ok(generation)
    }

    pub(super) fn start_wake(&mut self, generation: u64) -> bool {
        if matches!(self.lifecycle, Lifecycle::Open(Activity::WakeReserved(active)) if active == generation)
        {
            self.lifecycle = Lifecycle::Open(Activity::Active(generation));
            true
        } else {
            false
        }
    }

    /// Returns true only when this release claims deferred disposal.
    fn release(&mut self, generation: u64) -> bool {
        match self.lifecycle {
            Lifecycle::Open(Activity::WakeReserved(active) | Activity::Active(active))
                if active == generation =>
            {
                self.lifecycle = Lifecycle::Open(Activity::Idle);
                false
            }
            Lifecycle::CloseRequested {
                active: Some(active),
            } if active == generation => {
                self.lifecycle = Lifecycle::Disposing;
                true
            }
            _ => false,
        }
    }

    fn request_close(&mut self) {
        let active = match self.lifecycle {
            Lifecycle::Open(Activity::Active(generation)) => Some(generation),
            Lifecycle::Open(Activity::Idle | Activity::WakeReserved(_)) => None,
            _ => return,
        };
        self.lifecycle = Lifecycle::CloseRequested { active };
    }

    fn begin_disposal(&mut self) -> bool {
        if matches!(self.lifecycle, Lifecycle::CloseRequested { active: None }) {
            self.lifecycle = Lifecycle::Disposing;
            true
        } else {
            false
        }
    }

    fn finish_close(&mut self, result: Result<(), String>) {
        debug_assert!(matches!(self.lifecycle, Lifecycle::Disposing));
        self.lifecycle = Lifecycle::Closed(result);
    }

    fn close_result(&self) -> Option<&Result<(), String>> {
        match &self.lifecycle {
            Lifecycle::Closed(result) => Some(result),
            _ => None,
        }
    }

    /// Publication requires the same live admission that built the candidate.
    pub(super) fn install_replacement(
        &mut self,
        replacement: Current,
        generation: u64,
    ) -> ReplacementCommit {
        if matches!(self.lifecycle, Lifecycle::Open(Activity::Active(active)) if active == generation)
        {
            ReplacementCommit::Installed(self.current.replace(replacement))
        } else {
            ReplacementCommit::Rejected(replacement)
        }
    }
}

impl Controller {
    pub fn begin_operation(&self) -> Result<Admission<'_>, String> {
        let generation = self.lock().admit()?;
        Ok(Admission {
            controller: self,
            generation,
        })
    }

    pub(crate) fn begin_runner_operation(&self, runner: &Runner) -> Result<Admission<'_>, String> {
        let admission = self.begin_operation()?;
        let current = self.lock();
        let is_current = current
            .current
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.runner.as_ref(), runner));
        drop(current);
        if !is_current {
            return Err("the active session changed before the task resumed".into());
        }
        Ok(admission)
    }

    pub(super) fn release(&self, generation: u64) {
        let victim = {
            let mut state = self.lock();
            if !state.release(generation) {
                return;
            }
            state.current.take()
        };
        let _ = self.finish_close(victim);
    }

    fn finish_close(&self, victim: Option<Current>) -> Result<(), String> {
        let result = victim.map_or(Ok(()), Current::discard);
        self.lock().finish_close(result.clone());
        self.close_signal.notify_all();
        result
    }

    /// Rejects new work and hands the close to whichever operation is in
    /// flight. Never waits and never closes, so a callback running inside an
    /// operation may call it. An unstarted wake reservation is released.
    pub fn request_close(&self) {
        self.close_cancel.cancel();
        self.lock().request_close();
    }

    /// Completes the close, waiting for an in-flight operation to finish.
    /// Blocking: an async caller must cancel and await its own work first.
    pub fn close(&self) -> Result<(), String> {
        self.request_close();
        let victim = {
            let mut state = self.lock();
            loop {
                if let Some(result) = state.close_result() {
                    return result.clone();
                }
                if state.begin_disposal() {
                    break state.current.take();
                }
                state = self
                    .close_signal
                    .wait(state)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
        };
        self.finish_close(victim)
    }
}
