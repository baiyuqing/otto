//! The turn-level progress guard: how a turn stops when the model gets stuck.
//!
//! A turn whose model keeps issuing the same tool call, or keeps receiving tool
//! errors, is making no progress and would otherwise run until a deadline that
//! may be disabled. The guard keeps at most four result fingerprints to detect
//! short failed cycles as well as consecutive calls/errors. A separate bounded
//! advisory tracks single edits to at most 128 files per turn.
//!
//! Ownership: a [`ProgressGuard`] lives in
//! [`super::overflow::RunDispatchState`], which one `Agent::run` call owns and
//! drops, and is configured from [`super::Options::progress`]. Every turn uses
//! one, including each sub-agent's, so the parent loop and its children stop
//! under the same rule.
//!
//! Limitations: cycles longer than four calls and successful-only cycles are
//! not detected. Cycle tracking resets when an operation may have changed
//! state; result changes break a match. The agent uses consecutive checks only
//! when hooks are configured, since hook effects are not covered by tool safety.
//! Dependent-only waiting uses an explicit yield.
//! Unchanged counters alone do not establish that a task is stalled.

use std::collections::{BTreeMap, VecDeque, hash_map::DefaultHasher};
use std::fmt;
use std::hash::{Hash, Hasher};

use serde_json::Value;

use crate::model::{EffectCertainty, RetrySafety};
use crate::tool::ToolExecution;

const MAX_CYCLE_LENGTH: usize = 4;

/// Default repetitions of identical calls or unchanged failed short cycles.
pub const DEFAULT_REPEATED_CALLS: u32 = 8;
/// Default number of consecutive failed tool results that stops a turn.
pub const DEFAULT_CONSECUTIVE_FAILURES: u32 = 12;

/// The limits that stop a turn which makes no progress.
///
/// Zero disables one condition; a policy with both at zero never stops a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressPolicy {
    /// Identical consecutive tool calls, or unchanged failed 2–4-call cycles,
    /// that stop the turn after this many repetitions.
    pub repeated_calls: u32,
    /// Consecutive failed tool results that stop the turn.
    pub consecutive_failures: u32,
}

impl Default for ProgressPolicy {
    fn default() -> Self {
        Self {
            repeated_calls: DEFAULT_REPEATED_CALLS,
            consecutive_failures: DEFAULT_CONSECUTIVE_FAILURES,
        }
    }
}

impl ProgressPolicy {
    /// A policy that never stops a turn.
    pub const fn disabled() -> Self {
        Self {
            repeated_calls: 0,
            consecutive_failures: 0,
        }
    }

    /// Whether neither condition can stop a turn.
    pub const fn is_disabled(&self) -> bool {
        self.repeated_calls == 0 && self.consecutive_failures == 0
    }
}

/// Which limit a stalled turn hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallKind {
    /// The same tool call, with the same arguments, repeated in a row.
    RepeatedCall,
    /// Tool results failed in a row.
    RepeatedFailure,
    /// A short cycle with a failure and unchanged calls and results.
    RepeatedCycle,
}

impl StallKind {
    /// The stable name carried in machine-readable output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepeatedCall => "repeated_call",
            Self::RepeatedFailure => "repeated_failure",
            Self::RepeatedCycle => "repeated_cycle",
        }
    }
}

/// Why a turn was judged to be making no progress.
///
/// Frontends show the `Display` text verbatim, so it names the tool and the
/// count instead of only classifying the stall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    pub kind: StallKind,
    /// The tool of the last call.
    pub tool: String,
    /// Repeated calls, consecutive failures, or cycle repetitions.
    pub count: u32,
}

impl fmt::Display for Stall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            StallKind::RepeatedCall => write!(
                formatter,
                "the same {} call repeated {} times in a row",
                self.tool, self.count
            ),
            StallKind::RepeatedFailure => write!(
                formatter,
                "{} tool calls in a row failed (last: {})",
                self.count, self.tool
            ),
            StallKind::RepeatedCycle => write!(
                formatter,
                "a tool-call cycle repeated {} times without progress (last: {})",
                self.count, self.tool
            ),
        }
    }
}

/// Counts consecutive identical calls, failures, and short unchanged cycles.
#[derive(Debug, Default)]
pub struct ProgressGuard {
    policy: ProgressPolicy,
    /// The previous call's fingerprint, or `None` before the first call.
    previous: Option<u64>,
    /// Identical consecutive calls, including the current one.
    repetitions: u32,
    /// Consecutive failed results, including the current one.
    failures: u32,
    /// The first stall this turn hit. A stall is not undone by a later call,
    /// because the turn ends as soon as the caller reads it.
    pending: Option<Stall>,
    recent_results: VecDeque<(u64, bool)>,
    cycle_matches: [u32; MAX_CYCLE_LENGTH - 1],
    edits: BTreeMap<String, EditCount>,
}

#[derive(Debug, Default)]
struct EditCount {
    singles: u8,
    advised: bool,
}

impl ProgressGuard {
    pub fn new(policy: ProgressPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    pub fn policy(&self) -> ProgressPolicy {
        self.policy
    }

    /// Records one tool result and reports the stall it crossed, if any.
    ///
    /// `arguments` is the raw JSON the model sent; two spellings of the same
    /// object count as the same call.
    pub fn observe(&mut self, tool: &str, arguments: &str, is_error: bool) -> Option<Stall> {
        let fingerprint = fingerprint(tool, arguments);
        if self.previous == Some(fingerprint) {
            self.repetitions = self.repetitions.saturating_add(1);
        } else {
            self.previous = Some(fingerprint);
            self.repetitions = 1;
        }
        self.failures = if is_error {
            self.failures.saturating_add(1)
        } else {
            0
        };

        let crossed =
            if self.policy.repeated_calls > 0 && self.repetitions >= self.policy.repeated_calls {
                Some(Stall {
                    kind: StallKind::RepeatedCall,
                    tool: tool.to_string(),
                    count: self.repetitions,
                })
            } else if self.policy.consecutive_failures > 0
                && self.failures >= self.policy.consecutive_failures
            {
                Some(Stall {
                    kind: StallKind::RepeatedFailure,
                    tool: tool.to_string(),
                    count: self.failures,
                })
            } else {
                None
            };
        if crossed.is_some() {
            self.pending = crossed.clone();
        }
        crossed
    }

    /// Also compares results for short cycles. Only read-only operations and
    /// operations known not to have started can participate: a mutation or an
    /// uncertain effect may represent progress even when its output repeats.
    pub fn observe_execution(
        &mut self,
        tool: &str,
        arguments: &str,
        execution: &ToolExecution,
        safety: RetrySafety,
    ) -> Option<Stall> {
        let consecutive = self.observe(tool, arguments, execution.result.is_error);
        if self.policy.repeated_calls == 0
            || execution.outcome.effect_certainty == EffectCertainty::Unknown
            || (safety != RetrySafety::ReadOnly
                && execution.outcome.effect_certainty != EffectCertainty::NotStarted)
        {
            self.recent_results.clear();
            self.cycle_matches.fill(0);
            return consecutive;
        }

        let mut hasher = DefaultHasher::new();
        fingerprint(tool, arguments).hash(&mut hasher);
        execution.result.content.hash(&mut hasher);
        execution.result.is_error.hash(&mut hasher);
        let observation = (hasher.finish(), execution.result.is_error);
        for period in 2..=MAX_CYCLE_LENGTH {
            let matches = &mut self.cycle_matches[period - 2];
            let previous = self
                .recent_results
                .len()
                .checked_sub(period)
                .and_then(|index| self.recent_results.get(index));
            *matches = if previous == Some(&observation) {
                matches.saturating_add(1)
            } else {
                0
            };
        }
        self.recent_results.push_back(observation);
        if self.recent_results.len() > MAX_CYCLE_LENGTH {
            self.recent_results.pop_front();
        }
        if consecutive.is_some() {
            return consecutive;
        }
        for period in 2..=MAX_CYCLE_LENGTH {
            let repetitions = self.cycle_matches[period - 2] / period as u32 + 1;
            if repetitions >= self.policy.repeated_calls.max(2)
                && self
                    .recent_results
                    .iter()
                    .rev()
                    .take(period)
                    .any(|(_, is_error)| *is_error)
            {
                let stall = Stall {
                    kind: StallKind::RepeatedCycle,
                    tool: tool.into(),
                    count: repetitions,
                };
                self.pending.get_or_insert_with(|| stall.clone());
                return Some(stall);
            }
        }
        None
    }

    /// A one-time advisory per file after three successful single replacements.
    /// Reading another file does not reset it. Batches reset only the count;
    /// failures do not count, and all edits remain valid operations.
    pub fn edit_advice(
        &mut self,
        tool: &str,
        arguments: &str,
        is_error: bool,
    ) -> Option<&'static str> {
        if tool != "edit" || is_error {
            return None;
        }
        let args: Value = serde_json::from_str(arguments).ok()?;
        let path = args.get("path")?.as_str()?;
        let count = match args
            .get("edits")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
        {
            Some(items) => items.len(),
            None if args
                .get("old_text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty()) =>
            {
                1
            }
            _ => return None,
        };
        // Bound advisory state without changing execution for larger tasks.
        if self.edits.len() >= 128 && !self.edits.contains_key(path) {
            return None;
        }
        let entry = self.edits.entry(path.into()).or_default();
        if count > 1 {
            entry.singles = 0;
            return None;
        }
        entry.singles = entry.singles.saturating_add(1);
        if entry.singles >= 3 && !entry.advised {
            entry.advised = true;
            Some(
                "\nBatching advice: several single replacements were applied to this \
                    file. Collect the remaining known changes and apply one non-overlapping \
                    edits batch; use a checked script for repetitive cross-file changes. \
                    This edit succeeded.",
            )
        } else {
            None
        }
    }

    /// Takes the stall this turn hit, if it hit one.
    pub fn take_stall(&mut self) -> Option<Stall> {
        self.pending.take()
    }
}

/// A fingerprint of one tool call: the tool name and its canonical arguments.
///
/// The separator keeps `("ab", "c")` and `("a", "bc")` apart.
fn fingerprint(tool: &str, arguments: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    tool.hash(&mut hasher);
    '\0'.hash(&mut hasher);
    canonical_arguments(arguments).hash(&mut hasher);
    hasher.finish()
}

/// The arguments in one spelling, so whitespace and key order do not make two
/// identical calls look different.
///
/// The JSON object type does not sort its keys: `serde_json` keeps insertion
/// order when a native workspace build enables its `preserve_order` feature, so
/// the keys are sorted here instead of relying on the map's order. Arguments
/// that are not valid JSON are compared as sent.
pub fn canonical_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => {
            let mut canonical = String::with_capacity(arguments.len());
            write_canonical(&value, &mut canonical);
            canonical
        }
        Err(_) => arguments.to_owned(),
    }
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_json_string(key, out);
                out.push(':');
                write_canonical(value, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn push_json_string(text: &str, out: &mut String) {
    match serde_json::to_string(text) {
        Ok(quoted) => out.push_str(&quoted),
        Err(_) => out.push_str(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(content: &str, is_error: bool) -> ToolExecution {
        let mut execution = ToolExecution::completed(crate::tool::ToolResult {
            content: content.into(),
            is_error,
            ..crate::tool::ToolResult::default()
        });
        if is_error {
            execution.outcome.effect_certainty = EffectCertainty::NotStarted;
        }
        execution
    }

    #[test]
    fn unchanged_failed_cycles_stop_after_the_configured_repetitions() {
        for period in 2..=MAX_CYCLE_LENGTH {
            let mut guard = ProgressGuard::new(ProgressPolicy {
                repeated_calls: 3,
                consecutive_failures: 12,
            });
            for index in 0..period * 3 {
                let position = index % period;
                let execution = result("unchanged", position == 0);
                let stall = guard.observe_execution(
                    if position == 0 { "edit" } else { "read" },
                    &format!(r#"{{"position":{position}}}"#),
                    &execution,
                    if position == 0 {
                        RetrySafety::NonIdempotent
                    } else {
                        RetrySafety::ReadOnly
                    },
                );
                if index + 1 == period * 3 {
                    let stall = stall.expect("the unchanged failed cycle must stop");
                    assert_eq!(stall.kind, StallKind::RepeatedCycle);
                    assert_eq!(stall.count, 3);
                    assert_eq!(guard.take_stall(), Some(stall));
                } else {
                    assert!(stall.is_none(), "period {period}, call {index}: {stall:?}");
                }
            }
        }
    }

    #[test]
    fn changing_results_and_successful_cycles_do_not_count_as_failed_cycles() {
        for changed_position in [Some(0), Some(1), None] {
            let mut guard = ProgressGuard::default();
            for index in 0..100 {
                let position = index % 2;
                let content = if changed_position == Some(position) {
                    format!("revision {}", index / 2)
                } else {
                    "unchanged".into()
                };
                let execution = result(&content, changed_position.is_some() && position == 0);
                assert!(
                    guard
                        .observe_execution(
                            "read",
                            &format!(r#"{{"position":{position}}}"#),
                            &execution,
                            RetrySafety::ReadOnly,
                        )
                        .is_none()
                );
                assert!(guard.recent_results.len() <= MAX_CYCLE_LENGTH);
            }
        }
    }

    #[test]
    fn possible_mutations_reset_cycle_tracking() {
        for (is_error, certainty) in [
            (false, EffectCertainty::Completed),
            (true, EffectCertainty::Completed),
            (true, EffectCertainty::Unknown),
        ] {
            let mut guard = ProgressGuard::default();
            for _ in 0..20 {
                assert!(
                    guard
                        .observe_execution(
                            "read",
                            "{}",
                            &result("same", false),
                            RetrySafety::ReadOnly
                        )
                        .is_none()
                );
                let mut execution = result("same", is_error);
                execution.outcome.effect_certainty = certainty;
                assert!(
                    guard
                        .observe_execution("bash", "{}", &execution, RetrySafety::NonIdempotent)
                        .is_none()
                );
                assert!(guard.recent_results.is_empty());
            }
        }
    }

    #[test]
    fn cycles_respect_the_disabled_repetition_limit() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 0,
            consecutive_failures: 12,
        });
        for index in 0..100 {
            assert!(
                guard
                    .observe_execution(
                        "read",
                        &format!(r#"{{"position":{}}}"#, index % 2),
                        &result("same", index % 2 == 0),
                        RetrySafety::ReadOnly,
                    )
                    .is_none()
            );
        }
    }

    #[test]
    fn repeated_single_edits_warn_once_without_rejecting_or_counting_failures() {
        let mut guard = ProgressGuard::default();
        let single = r#"{"path":"a.rs","old_text":"x","new_text":"y"}"#;
        assert!(guard.edit_advice("edit", single, false).is_none());
        assert!(guard.edit_advice("read", single, false).is_none());
        assert!(guard.edit_advice("edit", single, true).is_none());
        assert!(guard.edit_advice("edit", single, false).is_none());
        assert!(
            guard
                .edit_advice("edit", single, false)
                .unwrap()
                .contains("This edit succeeded")
        );
        assert!(guard.edit_advice("edit", single, false).is_none());
        assert!(guard.take_stall().is_none());
    }

    #[test]
    fn batched_edits_reset_the_single_edit_streak() {
        let mut guard = ProgressGuard::default();
        let single = r#"{"path":"a.rs","edits":[{"old_text":"x","new_text":"y"}]}"#;
        for _ in 0..2 {
            assert!(guard.edit_advice("edit", single, false).is_none());
        }
        let batch = r#"{"path":"a.rs","edits":[{"old_text":"x","new_text":"y"},{"old_text":"z","new_text":"w"}]}"#;
        assert!(guard.edit_advice("edit", batch, false).is_none());
        for _ in 0..2 {
            assert!(guard.edit_advice("edit", single, false).is_none());
        }
        assert!(guard.edit_advice("edit", single, false).is_some());
    }

    #[test]
    fn identical_calls_trip_the_repeated_call_limit() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 3,
            consecutive_failures: 0,
        });
        assert!(
            guard
                .observe("read", r#"{"path":"a.txt"}"#, false)
                .is_none()
        );
        assert!(
            guard
                .observe("read", r#"{"path":"a.txt"}"#, false)
                .is_none()
        );
        let stall = guard
            .observe("read", r#"{"path":"a.txt"}"#, false)
            .expect("third identical call stops the turn");
        assert_eq!(stall.kind, StallKind::RepeatedCall);
        assert_eq!(stall.tool, "read");
        assert_eq!(stall.count, 3);
    }

    #[test]
    fn a_different_call_resets_the_repetition_count() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 3,
            consecutive_failures: 0,
        });
        for _ in 0..2 {
            assert!(
                guard
                    .observe("read", r#"{"path":"a.txt"}"#, false)
                    .is_none()
            );
        }
        assert!(guard.observe("grep", r#"{"pattern":"a"}"#, false).is_none());
        assert!(
            guard
                .observe("read", r#"{"path":"a.txt"}"#, false)
                .is_none()
        );
        assert!(
            guard
                .observe("read", r#"{"path":"a.txt"}"#, false)
                .is_none()
        );
        let stall = guard
            .observe("read", r#"{"path":"a.txt"}"#, false)
            .expect("three in a row after the interruption");
        assert_eq!(stall.count, 3);
    }

    #[test]
    fn the_same_arguments_in_another_spelling_are_the_same_call() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 2,
            consecutive_failures: 0,
        });
        assert!(
            guard
                .observe("write", r#"{"path":"a","content":"x"}"#, false)
                .is_none()
        );
        let stall = guard
            .observe("write", r#"{ "content" : "x" , "path" : "a" }"#, false)
            .expect("key order and whitespace do not make a new call");
        assert_eq!(stall.kind, StallKind::RepeatedCall);
    }

    #[test]
    fn the_same_tool_with_different_arguments_is_a_new_call() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 2,
            consecutive_failures: 0,
        });
        assert!(guard.observe("read", r#"{"path":"a"}"#, false).is_none());
        assert!(guard.observe("read", r#"{"path":"b"}"#, false).is_none());
        assert!(guard.observe("read", r#"{"path":"c"}"#, false).is_none());
    }

    #[test]
    fn nested_objects_are_canonicalized_too() {
        assert_eq!(
            canonical_arguments(r#"{"a":{"b":1,"a":2},"z":[1,{"y":2,"x":3}]}"#),
            r#"{"a":{"a":2,"b":1},"z":[1,{"x":3,"y":2}]}"#
        );
    }

    #[test]
    fn arguments_that_are_not_json_are_compared_as_sent() {
        assert_eq!(canonical_arguments("not json"), "not json");
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 2,
            consecutive_failures: 0,
        });
        assert!(guard.observe("bash", "", false).is_none());
        assert!(guard.observe("bash", "", false).is_some());
    }

    #[test]
    fn failing_results_trip_the_failure_limit() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 0,
            consecutive_failures: 3,
        });
        assert!(guard.observe("read", r#"{"path":"a"}"#, true).is_none());
        assert!(guard.observe("grep", r#"{"pattern":"b"}"#, true).is_none());
        let stall = guard
            .observe("find", r#"{"pattern":"c"}"#, true)
            .expect("a third different failure still stops the turn");
        assert_eq!(stall.kind, StallKind::RepeatedFailure);
        assert_eq!(stall.tool, "find");
        assert_eq!(stall.count, 3);
    }

    #[test]
    fn a_successful_result_resets_the_failure_streak() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 0,
            consecutive_failures: 3,
        });
        assert!(guard.observe("read", r#"{"path":"a"}"#, true).is_none());
        assert!(guard.observe("read", r#"{"path":"a"}"#, true).is_none());
        assert!(guard.observe("read", r#"{"path":"a"}"#, false).is_none());
        assert!(guard.observe("read", r#"{"path":"a"}"#, true).is_none());
        assert!(guard.observe("read", r#"{"path":"a"}"#, true).is_none());
        assert!(guard.observe("read", r#"{"path":"a"}"#, true).is_some());
    }

    #[test]
    fn a_guard_with_both_conditions_off_never_stops_a_turn() {
        let mut guard = ProgressGuard::new(ProgressPolicy::disabled());
        for _ in 0..100 {
            assert!(guard.observe("read", r#"{"path":"a"}"#, true).is_none());
        }
        assert!(guard.take_stall().is_none());
        assert!(ProgressPolicy::disabled().is_disabled());
    }

    #[test]
    fn the_first_stall_survives_a_later_different_call() {
        let mut guard = ProgressGuard::new(ProgressPolicy {
            repeated_calls: 2,
            consecutive_failures: 0,
        });
        assert!(guard.observe("read", r#"{"path":"a"}"#, false).is_none());
        assert!(guard.observe("read", r#"{"path":"a"}"#, false).is_some());
        assert!(guard.observe("grep", r#"{"pattern":"b"}"#, false).is_none());
        let stall = guard.take_stall().expect("the stall is still pending");
        assert_eq!(stall.kind, StallKind::RepeatedCall);
        assert!(guard.take_stall().is_none(), "taking it clears it");
    }

    #[test]
    fn the_stall_text_names_the_tool_and_the_count() {
        let stall = Stall {
            kind: StallKind::RepeatedCall,
            tool: "read".into(),
            count: 8,
        };
        assert_eq!(
            stall.to_string(),
            "the same read call repeated 8 times in a row"
        );
        let stall = Stall {
            kind: StallKind::RepeatedFailure,
            tool: "bash".into(),
            count: 12,
        };
        assert_eq!(
            stall.to_string(),
            "12 tool calls in a row failed (last: bash)"
        );
    }

    #[test]
    fn the_standard_policy_is_the_documented_default() {
        let policy = ProgressPolicy::default();
        assert_eq!(policy.repeated_calls, 8);
        assert_eq!(policy.consecutive_failures, 12);
        assert!(!policy.is_disabled());
    }

    #[test]
    fn tool_and_arguments_cannot_be_confused_across_calls() {
        // ("ab", "c") and ("a", "bc") are different calls.
        assert_ne!(fingerprint("ab", "c"), fingerprint("a", "bc"));
        assert_eq!(fingerprint("read", "{}"), fingerprint("read", "{}"));
    }
}
