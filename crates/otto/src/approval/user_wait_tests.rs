//! A wait for a person inside an agent run is not active time of its budget.

use std::sync::Arc;
use std::time::Duration;

use otto_core::agent::budget::{BudgetPolicy, ManualClock, RootBudget};
use otto_core::approval::{Authority, Kind, Scope, Subject};
use tokio_util::sync::CancellationToken;

use super::{Actor, Proposal, Service, user_wait};

fn scope() -> Scope {
    Scope {
        workspace: "/w".into(),
        session_id: "s".into(),
        task_id: String::new(),
        run_id: String::new(),
    }
}

fn confirmation(service: &Service) -> otto_core::approval::Request {
    service
        .create(Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Consent,
            scope: scope(),
            subject: Subject {
                tool_call_id: String::new(),
                source: "conversation".into(),
                id: "operation".into(),
                version: "v1".into(),
                details: "Plan".into(),
            },
            title: "Review plan".into(),
            reason: "Before proceeding".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap()
}

#[tokio::test]
async fn a_wait_for_a_person_inside_a_run_adds_no_active_time() {
    let service = Arc::new(Service::in_memory().unwrap());
    let request = confirmation(&service);
    let clock = ManualClock::new();
    let root = RootBudget::new("root-1", BudgetPolicy::unlimited(), clock.shared());
    let holder = root.enter();
    let cancel = CancellationToken::new();

    let waiting = user_wait::scoped(
        Some(holder.handle()),
        service.wait(&request.id, &request.scope, &cancel),
    );
    let person = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        clock.advance(Duration::from_secs(3600));
        assert_eq!(root.active_time(), Duration::ZERO, "an hour of waiting");
        service
            .decide(
                &request.id,
                &request.scope,
                request.revision,
                true,
                "",
                &Actor::local("frontend"),
            )
            .unwrap();
    };
    let (decided, ()) = tokio::join!(waiting, person);
    assert_eq!(decided.unwrap().state, otto_core::approval::State::Approved);

    // The run is working again: time counts once the wait is over.
    clock.advance(Duration::from_secs(5));
    assert_eq!(root.active_time(), Duration::from_secs(5));
}

#[tokio::test]
async fn a_wait_outside_a_run_pauses_nothing() {
    let service = Arc::new(Service::in_memory().unwrap());
    let request = confirmation(&service);
    let clock = ManualClock::new();
    let root = RootBudget::new("root-1", BudgetPolicy::unlimited(), clock.shared());
    let _holder = root.enter();
    let cancel = CancellationToken::new();

    let waiting = service.wait(&request.id, &request.scope, &cancel);
    let person = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        clock.advance(Duration::from_secs(60));
        service
            .decide(
                &request.id,
                &request.scope,
                request.revision,
                true,
                "",
                &Actor::local("frontend"),
            )
            .unwrap();
    };
    let (decided, ()) = tokio::join!(waiting, person);
    decided.unwrap();
    assert_eq!(root.active_time(), Duration::from_secs(60));
}
