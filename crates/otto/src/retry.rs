//! Pure retry-delay arithmetic for native transports.

use std::time::Duration;

/// Bounds for retry attempts and delays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    pub max_attempts: u32,
    pub base: Duration,
    pub max: Duration,
    pub retry_after_cap: Duration,
}

/// Supplies a full-jitter delay in the inclusive range from zero to `upper_bound`.
///
/// Callers can provide a deterministic implementation in tests. Values outside
/// the requested range are clamped before they are used.
pub trait JitterSource {
    fn full_jitter(&mut self, upper_bound: Duration) -> Duration;
}

/// Calculates the delay before the next attempt.
///
/// `attempt` is the one-based attempt that just completed. A `retry_after`
/// value takes precedence over exponential backoff and is capped by the
/// policy. The returned delay always leaves at least `min_execution_budget`
/// within `remaining_budget`.
pub fn next_delay(
    policy: Policy,
    attempt: u32,
    retry_after: Option<Duration>,
    remaining_budget: Duration,
    min_execution_budget: Duration,
    jitter: &mut impl JitterSource,
) -> Option<Duration> {
    if attempt == 0 || attempt >= policy.max_attempts {
        return None;
    }

    let delay = match retry_after {
        Some(delay) => delay.min(policy.retry_after_cap),
        None => {
            let upper_bound = exponential_bound(policy, attempt);
            jitter.full_jitter(upper_bound).min(upper_bound)
        }
    };

    let available_for_delay = remaining_budget.checked_sub(min_execution_budget)?;
    (delay <= available_for_delay).then_some(delay)
}

fn exponential_bound(policy: Policy, attempt: u32) -> Duration {
    let mut delay = policy.base.min(policy.max);
    if delay.is_zero() || delay == policy.max {
        return delay;
    }

    for _ in 1..attempt {
        delay = match delay.checked_mul(2) {
            Some(doubled) if doubled < policy.max => doubled,
            _ => return policy.max,
        };
    }

    delay
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedJitter(Duration);

    impl JitterSource for FixedJitter {
        fn full_jitter(&mut self, _upper_bound: Duration) -> Duration {
            self.0
        }
    }

    fn policy() -> Policy {
        Policy {
            max_attempts: 5,
            base: Duration::from_secs(2),
            max: Duration::from_secs(30),
            retry_after_cap: Duration::from_secs(20),
        }
    }

    #[test]
    fn exponential_backoff_saturates_without_overflow() {
        let policy = Policy {
            max_attempts: u32::MAX,
            base: Duration::MAX,
            max: Duration::MAX,
            retry_after_cap: Duration::MAX,
        };
        let mut jitter = FixedJitter(Duration::MAX);

        assert_eq!(
            next_delay(
                policy,
                200,
                None,
                Duration::MAX,
                Duration::ZERO,
                &mut jitter,
            ),
            Some(Duration::MAX)
        );
    }

    #[test]
    fn deterministic_full_jitter_is_bounded_by_exponential_delay() {
        let mut selected = FixedJitter(Duration::from_secs(7));
        assert_eq!(
            next_delay(
                policy(),
                3,
                None,
                Duration::from_secs(60),
                Duration::from_secs(1),
                &mut selected,
            ),
            Some(Duration::from_secs(7))
        );

        let mut out_of_range = FixedJitter(Duration::MAX);
        assert_eq!(
            next_delay(
                policy(),
                3,
                None,
                Duration::from_secs(60),
                Duration::from_secs(1),
                &mut out_of_range,
            ),
            Some(Duration::from_secs(8))
        );
    }

    #[test]
    fn retry_after_is_capped_before_budget_check_and_skips_jitter() {
        struct PanicJitter;

        impl JitterSource for PanicJitter {
            fn full_jitter(&mut self, _upper_bound: Duration) -> Duration {
                panic!("Retry-After must not use jitter")
            }
        }

        assert_eq!(
            next_delay(
                policy(),
                1,
                Some(Duration::from_secs(90)),
                Duration::from_secs(25),
                Duration::from_secs(5),
                &mut PanicJitter,
            ),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            next_delay(
                policy(),
                1,
                Some(Duration::from_secs(90)),
                Duration::from_secs(24),
                Duration::from_secs(5),
                &mut PanicJitter,
            ),
            None
        );
    }

    #[test]
    fn delay_must_leave_the_minimum_execution_budget() {
        let mut jitter = FixedJitter(Duration::from_secs(2));
        assert_eq!(
            next_delay(
                policy(),
                1,
                None,
                Duration::from_secs(7),
                Duration::from_secs(5),
                &mut jitter,
            ),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            next_delay(
                policy(),
                1,
                None,
                Duration::from_secs(6),
                Duration::from_secs(5),
                &mut jitter,
            ),
            None
        );
    }

    #[test]
    fn attempt_is_one_based_and_stops_at_max_attempts() {
        let mut jitter = FixedJitter(Duration::ZERO);

        assert_eq!(
            next_delay(
                policy(),
                0,
                None,
                Duration::MAX,
                Duration::ZERO,
                &mut jitter,
            ),
            None
        );
        assert_eq!(
            next_delay(
                policy(),
                4,
                None,
                Duration::MAX,
                Duration::ZERO,
                &mut jitter,
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(
            next_delay(
                policy(),
                5,
                None,
                Duration::MAX,
                Duration::ZERO,
                &mut jitter,
            ),
            None
        );
    }
}
