//! The fault vocabulary every decorator in [`crate::faults`] shares.
//!
//! A [`FaultPlan`] holds rules for one boundary's operation enum. A decorator
//! calls [`FaultPlan::check`] before each operation it forwards and applies
//! the returned [`Fault`]. The plan records every rule that fired, so a test
//! asserts both the order of faults and that each one fired.

use std::fmt::Debug;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Which calls of an operation a rule matches, by the subject the decorator
/// passes to [`FaultPlan::check`]: a file or journal name, an SQL statement,
/// an MCP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Any,
    /// The subject ends with this text.
    NameSuffix(&'static str),
    /// The subject, after leading whitespace, starts with this text, ASCII
    /// case-insensitively.
    SqlPrefix(&'static str),
}

impl Filter {
    fn matches(self, subject: &str) -> bool {
        match self {
            Self::Any => true,
            Self::NameSuffix(suffix) => subject.ends_with(suffix),
            Self::SqlPrefix(prefix) => subject
                .trim_start()
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix)),
        }
    }
}

/// Which of a rule's matching calls fire. Calls are counted per rule, after
/// its filter, starting at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Always,
    Nth(u32),
    FromNth(u32),
}

impl Trigger {
    fn fires(self, call: u32) -> bool {
        match self {
            Self::Always => true,
            Self::Nth(n) => call == n,
            Self::FromNth(n) => call >= n,
        }
    }
}

/// What the decorator does to the selected call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Fail without performing the operation. The effect certainty a caller
    /// may claim is `NotStarted`.
    Before(FaultError),
    /// Perform the operation, then report failure: a lost acknowledgement.
    /// The effect certainty a caller may claim is at most `Unknown`.
    After(FaultError),
    /// Writes only: persist the first `keep` bytes, then fail (torn write).
    Short { keep: usize, error: FaultError },
    /// Async boundaries only: never return. The caller's deadline or
    /// cancellation must end the call.
    Hang,
    /// Terminate the process before performing the operation. Only
    /// meaningful in a [`crate::faults::crash`] child.
    Exit,
    /// Perform the operation, then terminate the process before the caller
    /// sees the result.
    ExitAfter,
    /// Writes only: persist the first `keep` bytes, then terminate the
    /// process.
    ShortExit { keep: usize },
}

/// The error an injected failure reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultError {
    /// An OS error code such as `libc::EIO` or `libc::ENOSPC`.
    Os(i32),
    Kind(io::ErrorKind),
    /// A typed storage error, for SQL boundaries.
    Sql(SqlFault),
}

impl FaultError {
    /// The error as an [`io::Error`], for file and process boundaries.
    ///
    /// # Panics
    /// On [`FaultError::Sql`]: a plan that injects an SQL error into an I/O
    /// boundary is a test bug.
    pub fn io(self) -> io::Error {
        match self {
            Self::Os(code) => io::Error::from_raw_os_error(code),
            Self::Kind(kind) => io::Error::new(kind, "injected fault"),
            Self::Sql(fault) => panic!("SQL fault {fault:?} injected into an I/O boundary"),
        }
    }
}

/// The [`crate::storage::Error`] variants a fault can inject. The storage
/// error is not `Clone`, so a rule carries this and the SQL decorator builds
/// the error each time the rule fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlFault {
    Busy,
    BusySnapshot,
    Constraint,
    Corrupt,
    Unavailable,
    Closed,
    /// The commit outcome is unknown; never a rollback claim.
    CommitUnknown,
}

impl From<SqlFault> for crate::storage::Error {
    fn from(fault: SqlFault) -> Self {
        const MESSAGE: &str = "injected fault";
        match fault {
            SqlFault::Busy => Self::Busy(MESSAGE.into()),
            SqlFault::BusySnapshot => Self::BusySnapshot(MESSAGE.into()),
            SqlFault::Constraint => Self::Constraint(MESSAGE.into()),
            SqlFault::Corrupt => Self::Corrupt(MESSAGE.into()),
            SqlFault::Unavailable => Self::Unavailable,
            SqlFault::Closed => Self::Closed,
            SqlFault::CommitUnknown => Self::CommitUnknown,
        }
    }
}

/// One fault rule: inject `fault` into the calls of `op` that pass `filter`
/// and `trigger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule<Op> {
    pub op: Op,
    pub filter: Filter,
    pub trigger: Trigger,
    pub fault: Fault,
}

impl<Op> Rule<Op> {
    /// A rule over every call of `op`.
    pub fn new(op: Op, trigger: Trigger, fault: Fault) -> Self {
        Self {
            op,
            filter: Filter::Any,
            trigger,
            fault,
        }
    }

    pub fn filter(mut self, filter: Filter) -> Self {
        self.filter = filter;
        self
    }
}

/// A rule that fired: `call` is the rule's own matching-call count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fired<Op> {
    pub rule: usize,
    pub op: Op,
    pub call: u32,
    pub fault: Fault,
}

struct State<Op> {
    rules: Vec<(Rule<Op>, u32)>,
    fired: Vec<Fired<Op>>,
}

/// The rules for one boundary instance and the record of what fired.
///
/// A plan with rules must end with [`FaultPlan::assert_all_fired`]; in a
/// debug build, dropping it unchecked panics, unless the thread is already
/// panicking.
pub struct FaultPlan<Op> {
    state: Mutex<State<Op>>,
    checked: AtomicBool,
}

impl<Op: Copy + Eq + Debug> FaultPlan<Op> {
    pub fn new(rules: Vec<Rule<Op>>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                rules: rules.into_iter().map(|rule| (rule, 0)).collect(),
                fired: Vec::new(),
            }),
            checked: AtomicBool::new(false),
        })
    }

    /// A plan with no rules: the decorator forwards every call unchanged.
    pub fn none() -> Arc<Self> {
        Self::new(Vec::new())
    }

    /// Count this call of `op` against every rule it matches and return the
    /// fault of the first rule that fires. Later rules that would also fire
    /// on this call do not.
    pub fn check(&self, op: Op, subject: &str) -> Option<Fault> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut selected = None;
        for (index, (rule, calls)) in state.rules.iter_mut().enumerate() {
            if rule.op != op || !rule.filter.matches(subject) {
                continue;
            }
            *calls += 1;
            if selected.is_none() && rule.trigger.fires(*calls) {
                selected = Some(Fired {
                    rule: index,
                    op,
                    call: *calls,
                    fault: rule.fault,
                });
            }
        }
        let fired = selected?;
        state.fired.push(fired);
        Some(fired.fault)
    }

    /// Every fault that fired, in order.
    pub fn fired(&self) -> Vec<Fired<Op>> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fired
            .clone()
    }

    /// Panic unless every rule fired at least once. A rule that never fired
    /// would make the test pass without exercising its fault.
    pub fn assert_all_fired(&self) {
        self.checked.store(true, Ordering::Relaxed);
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let unfired: Vec<_> = state
            .rules
            .iter()
            .enumerate()
            .filter(|(index, _)| !state.fired.iter().any(|fired| fired.rule == *index))
            .map(|(index, (rule, calls))| format!("rule {index} {rule:?} ({calls} matching calls)"))
            .collect();
        assert!(
            unfired.is_empty(),
            "fault rules never fired:\n{}",
            unfired.join("\n")
        );
    }
}

impl<Op> Drop for FaultPlan<Op> {
    fn drop(&mut self) {
        let has_rules = !self
            .state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .rules
            .is_empty();
        if cfg!(debug_assertions)
            && has_rules
            && !*self.checked.get_mut()
            && !std::thread::panicking()
        {
            panic!("FaultPlan dropped without assert_all_fired()");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Op {
        Write,
        Sync,
    }

    const EIO: Fault = Fault::Before(FaultError::Os(libc::EIO));

    #[test]
    fn nth_counts_matching_calls_per_rule() {
        let plan = FaultPlan::new(vec![
            Rule::new(Op::Write, Trigger::Nth(2), EIO).filter(Filter::NameSuffix(".jsonl")),
        ]);
        assert_eq!(plan.check(Op::Write, "a.jsonl"), None);
        assert_eq!(plan.check(Op::Write, "a.lock"), None);
        assert_eq!(plan.check(Op::Sync, "a.jsonl"), None);
        assert_eq!(plan.check(Op::Write, "b.jsonl"), Some(EIO));
        assert_eq!(plan.check(Op::Write, "c.jsonl"), None);
        assert_eq!(
            plan.fired(),
            vec![Fired {
                rule: 0,
                op: Op::Write,
                call: 2,
                fault: EIO
            }]
        );
        plan.assert_all_fired();
    }

    #[test]
    fn from_nth_and_always_keep_firing() {
        let plan = FaultPlan::new(vec![
            Rule::new(Op::Sync, Trigger::FromNth(2), Fault::Hang),
            Rule::new(Op::Write, Trigger::Always, Fault::Exit),
        ]);
        assert_eq!(plan.check(Op::Sync, ""), None);
        assert_eq!(plan.check(Op::Sync, ""), Some(Fault::Hang));
        assert_eq!(plan.check(Op::Sync, ""), Some(Fault::Hang));
        assert_eq!(plan.check(Op::Write, ""), Some(Fault::Exit));
        assert_eq!(plan.fired().len(), 3);
        plan.assert_all_fired();
    }

    #[test]
    fn the_first_firing_rule_wins_and_later_rules_still_count() {
        let plan = FaultPlan::new(vec![
            Rule::new(Op::Write, Trigger::Nth(1), EIO),
            Rule::new(Op::Write, Trigger::Nth(2), Fault::Hang),
            Rule::new(Op::Write, Trigger::Nth(1), Fault::Exit),
        ]);
        assert_eq!(plan.check(Op::Write, ""), Some(EIO));
        assert_eq!(plan.check(Op::Write, ""), Some(Fault::Hang));
        let fired: Vec<_> = plan.fired().iter().map(|fired| fired.rule).collect();
        assert_eq!(fired, [0, 1]);
        let unfired = std::panic::catch_unwind(|| plan.assert_all_fired())
            .expect_err("rule 2 never fired");
        let message = unfired.downcast_ref::<String>().expect("panic message");
        assert!(message.contains("rule 2"), "{message}");
    }

    #[test]
    fn sql_prefix_ignores_leading_space_and_case() {
        let plan = FaultPlan::new(vec![
            Rule::new(Op::Write, Trigger::Always, EIO).filter(Filter::SqlPrefix("insert into")),
        ]);
        assert_eq!(plan.check(Op::Write, "\n  INSERT INTO t VALUES (1)"), Some(EIO));
        assert_eq!(plan.check(Op::Write, "SELECT 1"), None);
        assert_eq!(plan.check(Op::Write, "ins"), None);
        plan.assert_all_fired();
    }

    #[test]
    fn dropping_an_unchecked_plan_panics() {
        let plan = FaultPlan::new(vec![Rule::new(Op::Write, Trigger::Always, EIO)]);
        let dropped = std::panic::catch_unwind(move || drop(plan));
        assert!(dropped.is_err() == cfg!(debug_assertions));
        drop(FaultPlan::<Op>::none());
    }

    #[test]
    fn io_errors_keep_the_os_code() {
        assert_eq!(FaultError::Os(libc::ENOSPC).io().raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(
            FaultError::Kind(io::ErrorKind::BrokenPipe).io().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
