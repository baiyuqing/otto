use super::*;

fn usage(input: i64, output: i64) -> Option<Usage> {
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    })
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
    let mut budget = TurnBudget::new(policy(100, 0));
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
    let mut budget = TurnBudget::new(policy(10, 0));
    budget.charge(None);
    assert_eq!(budget.admit_tokens(), Ok(()));
}

#[test]
fn steps_are_counted_on_admission_and_refused_at_the_limit() {
    let mut budget = TurnBudget::new(policy(0, 3));
    for _ in 0..3 {
        assert_eq!(budget.admit_step(), Ok(()));
    }
    let exceeded = budget.admit_step().unwrap_err();
    assert_eq!(exceeded.kind, BudgetKind::Steps);
    assert_eq!((exceeded.used, exceeded.limit.get()), (3, 3));
}

#[test]
fn one_step_allows_exactly_one_call() {
    let mut budget = TurnBudget::new(policy(0, 1));
    assert_eq!(budget.admit_step(), Ok(()));
    assert!(budget.admit_step().is_err());
}

#[test]
fn unlimited_never_refuses() {
    let mut budget = TurnBudget::new(BudgetPolicy::unlimited());
    for _ in 0..1000 {
        budget.charge(usage(i64::MAX, i64::MAX));
        assert_eq!(budget.admit_step(), Ok(()));
    }
    assert_eq!(budget.admit_tokens(), Ok(()));
}

#[test]
fn the_two_limits_are_independent() {
    let mut tokens_only = TurnBudget::new(policy(0, 2));
    tokens_only.charge(usage(1_000_000, 1_000_000));
    assert_eq!(tokens_only.admit_step(), Ok(()));

    let mut steps_only = TurnBudget::new(policy(100, 0));
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
    let mut budget = TurnBudget::new(policy(10, 1));
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
