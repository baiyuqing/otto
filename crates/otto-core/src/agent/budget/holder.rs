//! The holders of a root: the work that runs while the root's active-time
//! clock runs.
//!
//! A [`Holder`] is one `Agent` run on the root: a root turn, an approval
//! continuation, a sub-agent turn, a workflow step attempt, a wake turn or a
//! follow-up turn. It is a guard: the clock counts the holder from
//! [`RootBudget::enter`] until the guard is dropped, including by an early
//! return, a cancellation or a panic.
//!
//! While a holder blocks on a user decision it holds a [`UserPause`]. A holder
//! counts as waiting once, however many pauses it holds at the same time (two
//! tool calls of one batch can both wait). The pause is taken from a
//! [`HolderHandle`], which the tools of the run reach through
//! [`crate::operation::OperationControl::holder`].

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::RootBudget;
use super::clock::Change;

/// What one holder contributes to the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HolderPhase {
    /// Running and not waiting.
    Running,
    /// Running, with this many pauses open.
    Waiting { pauses: NonZeroU32 },
    /// The holder ended. Pauses that are still open do nothing.
    Left,
}

#[derive(Debug)]
struct HolderShared {
    root: RootBudget,
    phase: Mutex<HolderPhase>,
}

impl HolderShared {
    fn phase(&self) -> MutexGuard<'_, HolderPhase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One running holder of a root. Dropping it ends the holder.
#[derive(Debug)]
pub struct Holder(Arc<HolderShared>);

impl Holder {
    pub(super) fn enter(root: &RootBudget) -> Self {
        root.change_clock(Change::Enter);
        Self(Arc::new(HolderShared {
            root: root.clone(),
            phase: Mutex::new(HolderPhase::Running),
        }))
    }

    /// A handle from which the run's tools take [`UserPause`]s.
    pub fn handle(&self) -> HolderHandle {
        HolderHandle(Arc::clone(&self.0))
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let mut phase = self.0.phase();
        let waiting = match *phase {
            HolderPhase::Running => false,
            HolderPhase::Waiting { .. } => true,
            HolderPhase::Left => return,
        };
        *phase = HolderPhase::Left;
        self.0.root.change_clock(Change::Leave { waiting });
    }
}

/// A clonable reference to a [`Holder`] that cannot end it.
#[derive(Debug, Clone)]
pub struct HolderHandle(Arc<HolderShared>);

impl HolderHandle {
    /// Marks the holder as waiting for a user decision until the returned
    /// guard is dropped. Nothing is counted for a holder that has ended.
    pub fn pause_for_user(&self) -> UserPause {
        let mut phase = self.0.phase();
        match *phase {
            HolderPhase::Running => {
                *phase = HolderPhase::Waiting {
                    pauses: NonZeroU32::MIN,
                };
                self.0.root.change_clock(Change::Wait);
            }
            HolderPhase::Waiting { pauses } => {
                *phase = HolderPhase::Waiting {
                    pauses: pauses.saturating_add(1),
                };
            }
            HolderPhase::Left => {}
        }
        UserPause(Arc::clone(&self.0))
    }
}

/// A holder waiting for a user decision. Dropping it ends the wait.
#[derive(Debug)]
pub struct UserPause(Arc<HolderShared>);

impl Drop for UserPause {
    fn drop(&mut self) {
        let mut phase = self.0.phase();
        match *phase {
            HolderPhase::Waiting { pauses } => match NonZeroU32::new(pauses.get() - 1) {
                Some(pauses) => *phase = HolderPhase::Waiting { pauses },
                None => {
                    *phase = HolderPhase::Running;
                    self.0.root.change_clock(Change::Resume);
                }
            },
            HolderPhase::Running | HolderPhase::Left => {}
        }
    }
}
