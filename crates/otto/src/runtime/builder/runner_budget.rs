//! The turn budget of one session as its runner sees it: which root a turn
//! starts, and whether a wake or follow-up turn may continue the current one.
//! See [`otto_core::agent::budget`].

use otto_core::agent::budget::{Exceeded, SkipReport, TurnBudget, WakeGate, Withheld};

use super::*;

impl Runner {
    /// The budget of a new root turn (a user, scheduled or chat input). It
    /// replaces the session's root, so tokens an earlier root used do not
    /// count against it.
    pub(crate) fn begin_root_turn(&self) -> TurnBudget {
        self.budget
            .begin_root(crate::subagent::new_budget_id(), self.budget_policy)
    }

    /// Decides one wake turn on the session's current root. With no root yet
    /// in this process the wake turn starts one. Nothing is written: a caller
    /// that withholds the turn calls [`Self::report_wake_withheld`].
    pub(crate) fn wake_gate(&self) -> WakeGate {
        self.budget
            .wake_gate(crate::subagent::new_budget_id, self.budget_policy)
    }

    /// Writes the `turn_budget_wake_skipped` record, once for each root: the
    /// record holds the root's id and the number of queued notifications, never
    /// their text.
    pub(crate) fn report_wake_withheld(&self, withheld: &Withheld) {
        if withheld.report == SkipReport::Written {
            return;
        }
        if let Some(diagnostics) = self.agent.diagnostics() {
            diagnostics.turn_budget_wake_skipped(&withheld.budget_id, self.mailbox().len());
        }
    }

    /// The budget of a follow-up turn: the session's current root and a step
    /// counter of its own, or the limit the root has already reached.
    pub(crate) fn followup_budget(&self) -> Result<TurnBudget, Exceeded> {
        if let Some(exceeded) = self.budget.exhausted() {
            return Err(exceeded);
        }
        Ok(self
            .budget
            .new_turn(crate::subagent::new_budget_id, self.budget_policy))
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Runner {
    /// Starts a root turn whose one-token limit is already used up, so a
    /// frontend test can reach the state that withholds wake turns without a
    /// provider that reports usage.
    pub fn exhaust_root_budget(&self) {
        use otto_core::agent::budget::{BudgetPolicy, Limit};
        let policy = BudgetPolicy {
            max_tokens: Limit::from_count(1),
            max_steps: Limit::Unlimited,
        };
        let turn = self.budget.begin_root("exhausted-for-tests".into(), policy);
        turn.charge(Some(otto_core::model::Usage {
            input_tokens: 1,
            ..otto_core::model::Usage::default()
        }));
    }
}
