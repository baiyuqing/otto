//! The turn budget: a hard bound on the tokens and provider steps that one
//! root turn and the work it starts may use.
//!
//! A [`BudgetPolicy`] holds two [`Limit`]s with different scopes:
//!
//! - `max_tokens` is shared. One [`RootBudget`] counts the tokens of a root
//!   turn and of every child that root starts (sub-agents, workflow steps, wake
//!   and follow-up turns). Every holder charges it and every holder sees its
//!   exhaustion.
//! - `max_steps` is per controller turn. A [`TurnBudget`] pairs the shared
//!   [`RootBudget`] with its own step counter. Every `Agent::run` that the same
//!   controller turn issues (the continuation after an approval, for example)
//!   uses the same [`TurnBudget`] and so continues the same step count. A child
//!   turn, a wake turn and a follow-up turn each get a new [`TurnBudget`] on the
//!   same root, with a step counter of their own.
//!
//! A holder is asked for admission before each ordinary provider call and
//! before each compaction request, so a limit stops a holder between calls and
//! never inside one. Nothing is cancelled when the shared tokens run out: each
//! holder stops at its own next check. The tokens of a response and the tool
//! batch that follows it can exceed `max_tokens`; with several holders the
//! overshoot is bounded by one more provider call and one tool batch for each
//! holder that was running.
//!
//! The caller chooses the scope of each run with a [`BudgetScope`]: a run that
//! has no controller turn (a test, a manual compaction) asks for
//! [`BudgetScope::Fresh`], which makes a new root for that run alone. A turn
//! that ends with [`super::AgentError::BudgetExceeded`] leaves the session
//! usable. Budgets live in memory only.

use std::fmt;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::model::Usage;

mod session;

pub use session::{SessionBudget, SessionBudgetCell, SkipReport, WakeGate, Withheld};

/// Default for `[agent.budget].max_tokens`: twice the p99 of 150 local turns.
pub const DEFAULT_MAX_TOKENS: u64 = 4_200_000;
/// Default for `[agent.budget].max_steps`: twice the p99 of 150 local turns.
pub const DEFAULT_MAX_STEPS: u64 = 54;

/// One bound. The configuration maps `0` to [`Limit::Unlimited`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The quantity is not bounded.
    Unlimited,
    /// The holder is refused further work once the quantity reaches this value.
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

/// The two limits a root turn and its controller turns run under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetPolicy {
    /// Tokens: the sum of input and output tokens over every provider
    /// response of the root turn and of everything it starts, compaction
    /// responses included. Shared by every holder of one [`RootBudget`].
    pub max_tokens: Limit,
    /// Ordinary provider calls of one controller turn, counted across its
    /// approval continuations; compaction requests are not steps. Each child,
    /// wake and follow-up turn counts its own steps.
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

/// A limit a holder reached: which one, its value and the amount used.
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

/// The phase of the shared token count. `Exhausted` is final: every later
/// request is refused with the first [`Exceeded`] that was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenPhase {
    Active { used: u64 },
    Exhausted(Exceeded),
}

/// The phase of one controller turn's step count. `Exhausted` is final for the
/// turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepPhase {
    Active { used: u64 },
    Exhausted(Exceeded),
}

#[derive(Debug)]
struct RootShared {
    id: String,
    policy: BudgetPolicy,
    tokens: Mutex<TokenPhase>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tokens of one root turn: a shared handle. Clones refer to the same
/// count, so a child that captured the handle sees every other holder's usage
/// and every holder sees the child's.
#[derive(Debug, Clone)]
pub struct RootBudget(Arc<RootShared>);

impl PartialEq for RootBudget {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for RootBudget {}

impl RootBudget {
    /// A root with nothing used. `id` names it in the log records of every
    /// holder, so the records of one root can be correlated; it is empty for
    /// the root of a [`BudgetScope::Fresh`] run.
    pub fn new(id: impl Into<String>, policy: BudgetPolicy) -> Self {
        Self(Arc::new(RootShared {
            id: id.into(),
            policy,
            tokens: Mutex::new(TokenPhase::Active { used: 0 }),
        }))
    }

    pub fn id(&self) -> &str {
        &self.0.id
    }

    pub fn policy(&self) -> BudgetPolicy {
        self.0.policy
    }

    /// A turn on this root with a step counter of its own.
    pub fn new_turn(&self) -> TurnBudget {
        TurnBudget {
            root: self.clone(),
            steps: Arc::new(Mutex::new(StepPhase::Active { used: 0 })),
        }
    }

    /// The tokens used so far, or the amount at which the limit was observed.
    pub fn tokens_used(&self) -> u64 {
        match *locked(&self.0.tokens) {
            TokenPhase::Active { used } => used,
            TokenPhase::Exhausted(exceeded) => exceeded.used,
        }
    }

    /// Adds `usage` (`input_tokens + output_tokens`; input already includes
    /// cached input) to the shared count. A response without usage adds
    /// nothing. Charging never refuses: the check is [`Self::admit_tokens`].
    pub fn charge(&self, usage: Option<Usage>) {
        let Some(usage) = usage else { return };
        let total = usage.input_tokens.max(0) as u64 + usage.output_tokens.max(0) as u64;
        if let TokenPhase::Active { used } = &mut *locked(&self.0.tokens) {
            *used = used.saturating_add(total);
        }
    }

    /// Whether another request that consumes tokens may start. The first
    /// refusal makes the root exhausted for every holder.
    pub fn admit_tokens(&self) -> Result<(), Exceeded> {
        let mut phase = locked(&self.0.tokens);
        match *phase {
            TokenPhase::Exhausted(exceeded) => Err(exceeded),
            TokenPhase::Active { used } => match self.0.policy.max_tokens.reached_by(used) {
                Some(limit) => {
                    let exceeded = Exceeded {
                        kind: BudgetKind::Tokens,
                        limit,
                        used,
                    };
                    *phase = TokenPhase::Exhausted(exceeded);
                    Err(exceeded)
                }
                None => Ok(()),
            },
        }
    }
}

/// The budget of one controller turn: the shared [`RootBudget`] and this
/// turn's own step count. Clones share both, so each `Agent::run` that a
/// controller turn issues can be given a clone and continues the same count.
#[derive(Debug, Clone)]
pub struct TurnBudget {
    root: RootBudget,
    steps: Arc<Mutex<StepPhase>>,
}

impl TurnBudget {
    pub fn root(&self) -> &RootBudget {
        &self.root
    }

    pub fn budget_id(&self) -> &str {
        self.root.id()
    }

    /// The provider steps counted so far in this turn.
    pub fn steps_used(&self) -> u64 {
        match *locked(&self.steps) {
            StepPhase::Active { used } => used,
            StepPhase::Exhausted(exceeded) => exceeded.used,
        }
    }

    /// See [`RootBudget::charge`].
    pub fn charge(&self, usage: Option<Usage>) {
        self.root.charge(usage);
    }

    /// Whether another request that consumes tokens may start. A turn whose
    /// steps ran out keeps reporting that limit, the first one it reached.
    pub fn admit_tokens(&self) -> Result<(), Exceeded> {
        if let StepPhase::Exhausted(exceeded) = *locked(&self.steps) {
            return Err(exceeded);
        }
        self.root.admit_tokens()
    }

    /// Whether the next ordinary provider call may start. On success the call
    /// counts as one step. Tokens are checked first.
    pub fn admit_step(&self) -> Result<(), Exceeded> {
        self.admit_tokens()?;
        let mut phase = locked(&self.steps);
        let used = match *phase {
            StepPhase::Exhausted(exceeded) => return Err(exceeded),
            StepPhase::Active { used } => used,
        };
        if let Some(limit) = self.root.policy().max_steps.reached_by(used) {
            let exceeded = Exceeded {
                kind: BudgetKind::Steps,
                limit,
                used,
            };
            *phase = StepPhase::Exhausted(exceeded);
            return Err(exceeded);
        }
        *phase = StepPhase::Active { used: used + 1 };
        Ok(())
    }
}

/// Which budget an `Agent` run uses. The caller always chooses one: no run
/// starts a budget implicitly.
#[derive(Debug, Clone)]
pub enum BudgetScope {
    /// A new root under this policy for this run alone, with an empty
    /// `budget_id`. For a run that has no controller turn: a test, a manual
    /// compaction.
    Fresh(BudgetPolicy),
    /// The budget of a controller turn. The run adds to its token count and
    /// continues its step count.
    Shared(TurnBudget),
}

impl BudgetScope {
    /// The turn budget the run starts with.
    pub fn into_turn(self) -> TurnBudget {
        match self {
            Self::Fresh(policy) => RootBudget::new("", policy).new_turn(),
            Self::Shared(turn) => turn,
        }
    }
}

#[cfg(test)]
mod tests;
