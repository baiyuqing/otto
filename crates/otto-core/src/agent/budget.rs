//! The per-turn budget: a hard bound on the tokens and provider steps one turn
//! may use.
//!
//! A [`BudgetPolicy`] holds two [`Limit`]s. A [`TurnBudget`] starts every turn
//! from a policy, is charged with the usage of each provider response
//! (including compaction responses) and is asked for admission before each
//! ordinary provider call, so a limit stops a turn between calls and never
//! inside one. The tokens of a response and the tool batch that follows it can
//! exceed `max_tokens`: that overshoot is accepted and is bounded by one
//! response plus its tool results.
//!
//! Ownership: one [`TurnBudget`] lives in each [`super::Agent`] and is replaced
//! at the start of every turn, so a sub-agent or workflow-step turn has its own
//! budget under the same policy. A turn that ends with
//! [`super::AgentError::BudgetExceeded`] leaves the session usable.

use std::fmt;
use std::num::NonZeroU64;

use crate::model::Usage;

/// Default for `[agent.budget].max_tokens`: twice the p99 of 150 local turns.
pub const DEFAULT_MAX_TOKENS: u64 = 4_200_000;
/// Default for `[agent.budget].max_steps`: twice the p99 of 150 local turns.
pub const DEFAULT_MAX_STEPS: u64 = 54;

/// One bound. The configuration maps `0` to [`Limit::Unlimited`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The quantity is not bounded.
    Unlimited,
    /// The turn is refused further work once the quantity reaches this value.
    At(NonZeroU64),
}

impl Limit {
    /// `0` is unlimited; any other value is a bound.
    pub const fn from_count(count: u64) -> Self {
        match NonZeroU64::new(count) {
            Some(limit) => Self::At(limit),
            None => Self::Unlimited,
        }
    }

    const fn reached_by(self, used: u64) -> Option<NonZeroU64> {
        match self {
            Self::At(limit) if used >= limit.get() => Some(limit),
            _ => None,
        }
    }
}

/// The two limits one turn runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetPolicy {
    /// Tokens: the sum of input and output tokens over every provider
    /// response of the turn, compaction responses included.
    pub max_tokens: Limit,
    /// Ordinary provider calls; compaction requests are not steps.
    pub max_steps: Limit,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            max_tokens: Limit::from_count(DEFAULT_MAX_TOKENS),
            max_steps: Limit::from_count(DEFAULT_MAX_STEPS),
        }
    }
}

impl BudgetPolicy {
    /// A policy that never stops a turn.
    pub const fn unlimited() -> Self {
        Self {
            max_tokens: Limit::Unlimited,
            max_steps: Limit::Unlimited,
        }
    }
}

/// Which quantity a stopped turn exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetKind {
    Tokens,
    Steps,
}

impl BudgetKind {
    /// The stable name carried in the log record's `limit` field.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::Steps => "steps",
        }
    }

    /// The configuration key that sets the limit.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Tokens => "[agent.budget].max_tokens",
            Self::Steps => "[agent.budget].max_steps",
        }
    }
}

/// A limit a turn reached: which one, its value and the amount used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exceeded {
    pub kind: BudgetKind,
    pub limit: NonZeroU64,
    pub used: u64,
}

impl fmt::Display for Exceeded {
    /// Frontends show this text verbatim, so it names the key to change.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = match self.kind {
            BudgetKind::Tokens => "tokens",
            BudgetKind::Steps => "provider steps",
        };
        write!(
            formatter,
            "{} of {} {unit} used in this turn ({})",
            self.used,
            self.limit,
            self.kind.key()
        )
    }
}

/// The phase of one turn's budget. `Exhausted` is final for the turn: every
/// later request is refused with the limit that was reached first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnBudget {
    Active {
        policy: BudgetPolicy,
        tokens: u64,
        steps: u64,
    },
    Exhausted(Exceeded),
}

impl Default for TurnBudget {
    fn default() -> Self {
        Self::new(BudgetPolicy::default())
    }
}

impl TurnBudget {
    /// A budget with nothing used.
    pub const fn new(policy: BudgetPolicy) -> Self {
        Self::Active {
            policy,
            tokens: 0,
            steps: 0,
        }
    }

    /// Adds `usage` (`input_tokens + output_tokens`; input already includes
    /// cached input) to the tokens used. A response without usage adds nothing.
    /// Charging never refuses: the check is [`Self::admit_tokens`].
    pub fn charge(&mut self, usage: Option<Usage>) {
        if let (Self::Active { tokens, .. }, Some(usage)) = (&mut *self, usage) {
            let total = usage.input_tokens.max(0) as u64 + usage.output_tokens.max(0) as u64;
            *tokens = tokens.saturating_add(total);
        }
    }

    /// Whether another request that consumes tokens may start.
    pub fn admit_tokens(&mut self) -> Result<(), Exceeded> {
        match *self {
            Self::Exhausted(exceeded) => Err(exceeded),
            Self::Active { policy, tokens, .. } => {
                if let Some(limit) = policy.max_tokens.reached_by(tokens) {
                    return Err(self.exhaust(BudgetKind::Tokens, limit, tokens));
                }
                Ok(())
            }
        }
    }

    /// Whether the next ordinary provider call may start. On success the call
    /// counts as one step. Tokens are checked first.
    pub fn admit_step(&mut self) -> Result<(), Exceeded> {
        self.admit_tokens()?;
        let Self::Active { policy, steps, .. } = self else {
            unreachable!("admit_tokens returns an error in the exhausted phase")
        };
        if let Some(limit) = policy.max_steps.reached_by(*steps) {
            let used = *steps;
            return Err(self.exhaust(BudgetKind::Steps, limit, used));
        }
        *steps += 1;
        Ok(())
    }

    fn exhaust(&mut self, kind: BudgetKind, limit: NonZeroU64, used: u64) -> Exceeded {
        let exceeded = Exceeded { kind, limit, used };
        *self = Self::Exhausted(exceeded);
        exceeded
    }
}

#[cfg(test)]
mod tests;
