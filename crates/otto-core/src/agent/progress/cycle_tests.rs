//! Cycle detection: successful and failed cycles, the length bound, resets.

use super::*;

fn ok(content: &str) -> ToolExecution {
    ToolExecution::completed(crate::tool::ToolResult {
        content: content.into(),
        ..crate::tool::ToolResult::default()
    })
}

/// Feeds `calls` read-only calls cycling over `period` positions. `content`
/// maps (call index, position) to the result text. Returns the 1-based index of
/// the call that produced a stall and the stall.
fn run_cycle(
    policy: ProgressPolicy,
    period: usize,
    calls: usize,
    content: impl Fn(usize, usize) -> String,
) -> Option<(usize, Stall)> {
    let mut guard = ProgressGuard::new(policy);
    for index in 0..calls {
        let position = index % period;
        let stall = guard.observe_execution(
            "read",
            &format!(r#"{{"position":{position}}}"#),
            &ok(&content(index, position)),
            RetrySafety::ReadOnly,
        );
        if let Some(stall) = stall {
            return Some((index + 1, stall));
        }
        assert!(guard.recent_results.len() <= MAX_CYCLE_LENGTH);
    }
    None
}

fn policy(repeated_calls: u32) -> ProgressPolicy {
    ProgressPolicy {
        repeated_calls,
        consecutive_failures: 12,
    }
}

#[test]
fn a_successful_unchanged_cycle_stops_at_the_threshold_and_not_before() {
    let (call, stall) = run_cycle(policy(3), 2, 100, |_, _| "same".into())
        .expect("an unchanged successful cycle stops the turn");
    assert_eq!(call, 6, "3 repetitions of a 2-call period");
    assert_eq!(stall.kind, StallKind::RepeatedCycle);
    assert_eq!(stall.count, 3);
    assert!(run_cycle(policy(3), 2, 5, |_, _| "same".into()).is_none());
}

#[test]
fn a_cycle_whose_result_changes_every_round_never_stops() {
    for changing in [0, 1] {
        let found = run_cycle(policy(3), 2, 200, |index, position| {
            if position == changing {
                format!("revision {}", index / 2)
            } else {
                "same".into()
            }
        });
        assert!(found.is_none(), "changing position {changing}: {found:?}");
    }
}

#[test]
fn cycles_up_to_eight_calls_are_detected_and_longer_ones_are_not() {
    for period in 2..=MAX_CYCLE_LENGTH {
        let (call, stall) = run_cycle(policy(3), period, 1000, |_, _| "same".into())
            .unwrap_or_else(|| panic!("period {period} must be detected"));
        assert_eq!(call, period * 3);
        assert_eq!(stall.count, 3);
    }
    let longer = MAX_CYCLE_LENGTH + 1;
    assert!(run_cycle(policy(3), longer, 1000, |_, _| "same".into()).is_none());
    assert_eq!(MAX_CYCLE_LENGTH, 8);
}

#[test]
fn a_state_changing_operation_in_the_cycle_resets_detection() {
    let mut guard = ProgressGuard::new(policy(3));
    for index in 0..100 {
        let position = index % 3;
        let (safety, execution) = if position == 2 {
            (RetrySafety::NonIdempotent, ok("same"))
        } else {
            (RetrySafety::ReadOnly, ok("same"))
        };
        let stall = guard.observe_execution(
            "tool",
            &format!(r#"{{"position":{position}}}"#),
            &execution,
            safety,
        );
        assert!(stall.is_none(), "call {index}: {stall:?}");
    }
}

#[test]
fn the_disabled_repetition_limit_ignores_successful_cycles() {
    assert!(run_cycle(policy(0), 2, 200, |_, _| "same".into()).is_none());
}
