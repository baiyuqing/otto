use super::*;

fn usage(input: i64, output: i64) -> Option<Usage> {
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    })
}

/// A turn on a new root with the given limits.
fn turn(policy: BudgetPolicy) -> TurnBudget {
    RootBudget::new("root-1", policy).new_turn()
}

fn policy(tokens: u64, steps: u64) -> BudgetPolicy {
    BudgetPolicy {
        max_tokens: Limit::from_count(tokens),
        max_steps: Limit::from_count(steps),
    }
}

#[test]
fn zero_maps_to_unlimited_and_other_values_to_a_bound() {
    assert_eq!(Limit::from_count(0), Limit::Unlimited);
    assert_eq!(Limit::from_count(5), Limit::At(NonZeroU64::new(5).unwrap()));
}

#[test]
fn the_defaults_are_the_documented_values() {
    let defaults = BudgetPolicy::default();
    assert_eq!(defaults.max_tokens, Limit::from_count(4_200_000));
    assert_eq!(defaults.max_steps, Limit::from_count(54));
}

#[test]
fn tokens_are_refused_at_exactly_the_limit_and_not_below() {
    let budget = turn(policy(100, 0));
    budget.charge(usage(60, 39));
    assert_eq!(budget.admit_tokens(), Ok(()));
    budget.charge(usage(0, 1));
    let exceeded = budget.admit_tokens().unwrap_err();
    assert_eq!(exceeded.kind, BudgetKind::Tokens);
    assert_eq!(exceeded.used, 100);
    assert_eq!(exceeded.limit.get(), 100);
}

#[test]
fn a_response_without_usage_charges_nothing() {
    let budget = turn(policy(10, 0));
    budget.charge(None);
    assert_eq!(budget.admit_tokens(), Ok(()));
}

#[test]
fn steps_are_counted_on_admission_and_refused_at_the_limit() {
    let budget = turn(policy(0, 3));
    for _ in 0..3 {
        assert_eq!(budget.admit_step(), Ok(()));
    }
    let exceeded = budget.admit_step().unwrap_err();
    assert_eq!(exceeded.kind, BudgetKind::Steps);
    assert_eq!((exceeded.used, exceeded.limit.get()), (3, 3));
}

#[test]
fn one_step_allows_exactly_one_call() {
    let budget = turn(policy(0, 1));
    assert_eq!(budget.admit_step(), Ok(()));
    assert!(budget.admit_step().is_err());
}

#[test]
fn unlimited_never_refuses() {
    let budget = turn(BudgetPolicy::unlimited());
    for _ in 0..1000 {
        budget.charge(usage(i64::MAX, i64::MAX));
        assert_eq!(budget.admit_step(), Ok(()));
    }
    assert_eq!(budget.admit_tokens(), Ok(()));
}

#[test]
fn the_two_limits_are_independent() {
    let tokens_only = turn(policy(0, 2));
    tokens_only.charge(usage(1_000_000, 1_000_000));
    assert_eq!(tokens_only.admit_step(), Ok(()));

    let steps_only = turn(policy(100, 0));
    for _ in 0..50 {
        assert_eq!(steps_only.admit_step(), Ok(()));
    }
    steps_only.charge(usage(100, 0));
    assert_eq!(
        steps_only.admit_step().unwrap_err().kind,
        BudgetKind::Tokens
    );
}

#[test]
fn an_exhausted_budget_keeps_reporting_the_first_limit() {
    let budget = turn(policy(10, 1));
    assert_eq!(budget.admit_step(), Ok(()));
    let first = budget.admit_step().unwrap_err();
    assert_eq!(first.kind, BudgetKind::Steps);
    budget.charge(usage(100, 0));
    assert_eq!(budget.admit_tokens().unwrap_err(), first);
    assert_eq!(budget.admit_step().unwrap_err(), first);
}

#[test]
fn the_error_text_names_the_key_the_limit_and_the_amount_used() {
    let tokens = Exceeded {
        kind: BudgetKind::Tokens,
        limit: NonZeroU64::new(4_200_000).unwrap(),
        used: 4_200_311,
    };
    assert_eq!(
        tokens.to_string(),
        "4200311 of 4200000 tokens used in this turn ([agent.budget].max_tokens)"
    );
    let steps = Exceeded {
        kind: BudgetKind::Steps,
        limit: NonZeroU64::new(54).unwrap(),
        used: 54,
    };
    assert_eq!(
        steps.to_string(),
        "54 of 54 provider steps used in this turn ([agent.budget].max_steps)"
    );
}

#[test]
fn tokens_are_shared_by_every_turn_of_one_root() {
    let root = RootBudget::new("root-1", policy(100, 0));
    let parent = root.new_turn();
    let child = root.new_turn();
    parent.charge(usage(60, 0));
    assert_eq!(child.admit_tokens(), Ok(()));
    child.charge(usage(30, 20));
    // The child's usage is visible to the parent and the parent's to the child.
    let from_parent = parent.admit_tokens().unwrap_err();
    let from_child = child.admit_tokens().unwrap_err();
    assert_eq!(from_parent, from_child);
    assert_eq!(from_parent.kind, BudgetKind::Tokens);
    assert_eq!(from_parent.used, 110);
    assert_eq!(root.tokens_used(), 110);
}

#[test]
fn a_root_exhausted_by_one_holder_stays_exhausted_with_its_first_amount() {
    let root = RootBudget::new("root-1", policy(10, 0));
    let first = root.new_turn();
    let second = root.new_turn();
    first.charge(usage(10, 0));
    let exceeded = first.admit_step().unwrap_err();
    // Later usage of another holder does not change what is reported.
    second.charge(usage(500, 0));
    assert_eq!(second.admit_step().unwrap_err(), exceeded);
    assert_eq!(exceeded.used, 10);
}

#[test]
fn steps_belong_to_one_turn_and_a_clone_continues_them() {
    let root = RootBudget::new("root-1", policy(0, 3));
    let turn = root.new_turn();
    let continuation = turn.clone();
    assert_eq!(turn.admit_step(), Ok(()));
    assert_eq!(continuation.admit_step(), Ok(()));
    assert_eq!(turn.admit_step(), Ok(()));
    // The continuation shares the count: the fourth step is refused.
    assert_eq!(
        continuation.admit_step().unwrap_err().kind,
        BudgetKind::Steps
    );
    // Another turn on the same root counts its own steps.
    let other = root.new_turn();
    assert_eq!(other.steps_used(), 0);
    assert_eq!(other.admit_step(), Ok(()));
}

#[test]
fn a_fresh_scope_makes_a_new_root_each_time_and_a_shared_scope_keeps_its_own() {
    let first = BudgetScope::Fresh(policy(100, 0)).into_turn();
    first.charge(usage(100, 0));
    assert!(first.admit_tokens().is_err());
    let second = BudgetScope::Fresh(policy(100, 0)).into_turn();
    assert_eq!(second.admit_tokens(), Ok(()));
    assert_eq!(second.budget_id(), "");

    let shared = RootBudget::new("root-9", policy(100, 0)).new_turn();
    let scoped = BudgetScope::Shared(shared.clone()).into_turn();
    assert_eq!(scoped.budget_id(), "root-9");
    shared.charge(usage(100, 0));
    assert!(scoped.admit_tokens().is_err());
}

mod session {
    use super::*;

    fn ids() -> impl FnOnce() -> String {
        || "generated".to_string()
    }

    #[test]
    fn a_wake_with_no_root_yet_starts_a_root_that_later_children_share() {
        let cell = SessionBudgetCell::default();
        let WakeGate::Admit(turn) = cell.wake_gate(ids(), policy(100, 0)) else {
            panic!("a session with no root admits a wake turn");
        };
        assert_eq!(turn.budget_id(), "generated");
        let child = cell.current_root(|| unreachable!("the root exists"), policy(100, 0));
        assert_eq!(&child, turn.root());
    }

    #[test]
    fn each_user_turn_replaces_the_root() {
        let cell = SessionBudgetCell::default();
        let first = cell.begin_root("a".into(), policy(100, 0));
        first.charge(usage(100, 0));
        assert!(cell.exhausted().is_some());
        let second = cell.begin_root("b".into(), policy(100, 0));
        assert_eq!(second.budget_id(), "b");
        assert!(cell.exhausted().is_none());
        assert_ne!(first.root(), second.root());
    }

    #[test]
    fn an_exhausted_root_withholds_wake_turns_and_reports_once() {
        let cell = SessionBudgetCell::default();
        let root = cell.begin_root("a".into(), policy(100, 0));
        assert!(matches!(
            cell.wake_gate(ids(), policy(100, 0)),
            WakeGate::Admit(_)
        ));
        root.charge(usage(100, 0));

        let WakeGate::Withheld(first) = cell.wake_gate(ids(), policy(100, 0)) else {
            panic!("the exhausted root withholds the wake turn");
        };
        assert_eq!(first.budget_id, "a");
        assert_eq!(first.exceeded.kind, BudgetKind::Tokens);
        assert_eq!(first.report, SkipReport::Due);
        let WakeGate::Withheld(second) = cell.wake_gate(ids(), policy(100, 0)) else {
            panic!("still withheld");
        };
        assert_eq!(second.report, SkipReport::Written);

        // The next user turn starts a new root and the report is due again.
        let next = cell.begin_root("b".into(), policy(100, 0));
        assert!(matches!(
            cell.wake_gate(ids(), policy(100, 0)),
            WakeGate::Admit(_)
        ));
        next.charge(usage(100, 0));
        let WakeGate::Withheld(third) = cell.wake_gate(ids(), policy(100, 0)) else {
            panic!("the new root is exhausted");
        };
        assert_eq!(
            (third.budget_id.as_str(), third.report),
            ("b", SkipReport::Due)
        );
    }

    #[test]
    fn a_step_limit_never_withholds_a_wake_turn() {
        let cell = SessionBudgetCell::default();
        let turn = cell.begin_root("a".into(), policy(0, 1));
        assert_eq!(turn.admit_step(), Ok(()));
        assert!(turn.admit_step().is_err());
        // Steps are per controller turn: the wake turn counts its own.
        let WakeGate::Admit(wake) = cell.wake_gate(ids(), policy(0, 1)) else {
            panic!("steps do not withhold a wake turn");
        };
        assert_eq!(wake.admit_step(), Ok(()));
    }
}
