//! The root turn of one session: which [`RootBudget`] a wake turn, a follow-up
//! turn or a newly started child continues.
//!
//! A [`SessionBudgetCell`] lives as long as the session. A user-origin turn
//! replaces its root; everything else reads it. The state is in memory only, so
//! a session loaded by a new process starts at [`SessionBudget::NoRootYet`].

use std::sync::{Mutex, PoisonError};

use super::{BudgetPolicy, Exceeded, RootBudget, TurnBudget};

/// Whether the report of a withheld wake still has to be written. One report
/// is written for each root, not for each skipped attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReport {
    /// No record has been written for this root yet.
    Due,
    /// A record was already written for this root.
    Written,
}

/// The root turn the session continues.
#[derive(Debug, Clone)]
pub enum SessionBudget {
    /// No root turn has run in this process. The first wake turn, follow-up
    /// turn or child starts a root of its own.
    NoRootYet,
    /// The most recent root turn.
    Root {
        root: RootBudget,
        wake_skip: SkipReport,
    },
}

/// A wake turn the shared token budget does not admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    pub budget_id: String,
    pub exceeded: Exceeded,
    pub report: SkipReport,
}

/// The decision for one wake turn.
#[derive(Debug, Clone)]
pub enum WakeGate {
    /// The wake turn runs with this budget: the session's root and a step
    /// counter of its own.
    Admit(TurnBudget),
    /// The root's tokens are exhausted: the wake turn does not start and its
    /// notifications stay queued for the next user input.
    Withheld(Withheld),
}

/// The session's [`SessionBudget`] behind a lock.
#[derive(Debug)]
pub struct SessionBudgetCell {
    state: Mutex<SessionBudget>,
}

impl Default for SessionBudgetCell {
    fn default() -> Self {
        Self {
            state: Mutex::new(SessionBudget::NoRootYet),
        }
    }
}

impl SessionBudgetCell {
    fn lock(&self) -> std::sync::MutexGuard<'_, SessionBudget> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the root of a user-origin turn: the new root replaces the
    /// session's previous one. `id` names it in log records.
    pub fn begin_root(&self, id: String, policy: BudgetPolicy) -> TurnBudget {
        let root = RootBudget::new(id, policy);
        let turn = root.new_turn();
        *self.lock() = SessionBudget::Root {
            root,
            wake_skip: SkipReport::Due,
        };
        turn
    }

    /// The session's root, started with `id` and `policy` when no root turn
    /// has run yet. A child or workflow run captures it when it starts.
    pub fn current_root(&self, id: impl FnOnce() -> String, policy: BudgetPolicy) -> RootBudget {
        let mut state = self.lock();
        match &*state {
            SessionBudget::Root { root, .. } => root.clone(),
            SessionBudget::NoRootYet => {
                let root = RootBudget::new(id(), policy);
                *state = SessionBudget::Root {
                    root: root.clone(),
                    wake_skip: SkipReport::Due,
                };
                root
            }
        }
    }

    /// A turn on the session's root with a step counter of its own: for a
    /// follow-up turn. Unlike [`Self::wake_gate`] it neither refuses nor
    /// records anything; the caller decides with [`Self::exhausted`].
    pub fn new_turn(&self, id: impl FnOnce() -> String, policy: BudgetPolicy) -> TurnBudget {
        self.current_root(id, policy).new_turn()
    }

    /// The limit the session's root has reached, when it has one. The check
    /// itself exhausts the root for every holder.
    pub fn exhausted(&self) -> Option<Exceeded> {
        match &*self.lock() {
            SessionBudget::NoRootYet => None,
            SessionBudget::Root { root, .. } => root.admit_tokens().err(),
        }
    }

    /// Decides one wake turn. With no root yet, the wake turn starts one.
    /// A refusal marks its report written, so the caller that sees
    /// [`SkipReport::Due`] writes the one record of this root.
    pub fn wake_gate(&self, id: impl FnOnce() -> String, policy: BudgetPolicy) -> WakeGate {
        let mut state = self.lock();
        let (root, wake_skip) = match &mut *state {
            SessionBudget::Root { root, wake_skip } => (root.clone(), wake_skip),
            SessionBudget::NoRootYet => {
                let root = RootBudget::new(id(), policy);
                *state = SessionBudget::Root {
                    root: root.clone(),
                    wake_skip: SkipReport::Due,
                };
                return WakeGate::Admit(root.new_turn());
            }
        };
        match root.admit_tokens() {
            Ok(()) => WakeGate::Admit(root.new_turn()),
            Err(exceeded) => {
                let report = *wake_skip;
                *wake_skip = SkipReport::Written;
                WakeGate::Withheld(Withheld {
                    budget_id: root.id().to_owned(),
                    exceeded,
                    report,
                })
            }
        }
    }
}
