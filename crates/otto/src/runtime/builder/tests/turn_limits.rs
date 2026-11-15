use super::*;

/// Answers only once the turn is cancelled, so the test decides exactly
/// when guidance is still waiting for a safe boundary.
struct CancelledProvider;

#[async_trait::async_trait]
impl Provider for CancelledProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        control.cancellation_token().cancelled().await;
        ProviderSettlement::failed(
            otto_core::provider::ProviderError::Cancelled,
            1,
            EffectCertainty::Unknown,
        )
    }
}

/// Guidance a cancelled turn accepted and never delivered must come back
/// with its own words: the user typed it, the turn that would have carried
/// it is gone, and before this the guidance simply vanished.
#[tokio::test]
async fn a_cancelled_turn_reports_the_guidance_it_never_delivered() {
    use crate::subagent::tasks::Tasks;

    let runner = Runner::scripted(
        SharedSession::memory(Header::default()),
        Arc::new(CancelledProvider),
        Arc::new(Tasks::new()),
    );
    let steering = runner.steering();
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let run = async {
        // Esc only means something once the run accepts guidance.
        while steering.ticket().is_none() {
            tokio::task::yield_now().await;
        }
        steering.submit("keep my exact words").expect("active run");
        cancel.cancel();
    };
    let mut sink = |event| events.push(event);
    let (result, ()) = tokio::join!(runner.run("hello", &mut sink, &cancel), run);

    assert!(result.is_err(), "a cancelled turn reports its error");
    let notice = runner
        .session()
        .messages()
        .into_iter()
        .find(|message| message.context_type == "agent_steering")
        .expect("persisted steering-loss notice");
    assert_eq!(notice.role, otto_core::model::Role::Context);
    assert!(notice.display, "the loss must be visible, not hidden");
    assert!(
        notice.text().contains("keep my exact words"),
        "the notice must quote the guidance: {}",
        notice.text()
    );
    assert!(
        matches!(events.as_slice(), [.., Event::Notification { task_id, text, .. }] if task_id == "agent_steering" && text == &notice.text()),
        "the live event must say what the session recorded: {events:?}"
    );
}

/// A turn that delivers its guidance loses nothing, so it adds no notice.
#[tokio::test]
async fn a_turn_that_delivers_its_guidance_reports_no_loss() {
    use crate::fixtures::subagent_doubles::{FakeProvider, assistant_text, match_any};
    use crate::subagent::tasks::Tasks;
    use otto_core::model::Usage;

    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let runner = Runner::scripted(
        SharedSession::memory(Header::default()),
        provider,
        Arc::new(Tasks::new()),
    );
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let mut sink = |event| events.push(event);
    runner
        .run("hello", &mut sink, &cancel)
        .await
        .expect("an answered turn succeeds");

    assert!(
        runner
            .session()
            .messages()
            .iter()
            .all(|message| message.context_type != "agent_steering"),
        "an ordinary turn must not claim it lost guidance"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::Notification { task_id, .. } if task_id == "agent_steering"
        )),
        "{events:?}"
    );
}

#[test]
fn turn_limit_takes_the_earlier_limit_only_when_unattended() {
    let m = |n: u64| Some(Duration::from_secs(n * 60));
    for (turn, unattended_limit) in [(m(10), m(30)), (None, m(30)), (None, None)] {
        assert_eq!(turn_limit(turn, unattended_limit, false), turn);
    }
    assert_eq!(turn_limit(None, m(30), true), m(30));
    assert_eq!(turn_limit(m(10), m(30), true), m(10));
    assert_eq!(turn_limit(m(60), m(30), true), m(30));
    assert_eq!(turn_limit(m(60), None, true), m(60));
    assert_eq!(turn_limit(None, None, true), None);
}

/// Answers only when the turn's control stops the operation; the scripted
/// client bypasses `complete_with_timeout`, so it must observe the stop.
struct UntilStopped;

#[async_trait::async_trait]
impl Provider for UntilStopped {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        control.cancellation_token().cancelled().await;
        provider_stop_settlement(
            control
                .stop_reason()
                .unwrap_or(OperationStopReason::Deadline),
        )
    }
}

/// Runs one turn against a provider that never answers, advancing paused
/// time past the unattended deadline plus the cancellation grace.
async fn unattended_turn_ends(origin: otto_session::origin::Origin, headless: bool) -> bool {
    let mut runner = Runner::scripted(
        SharedSession::memory(Header::default()),
        Arc::new(UntilStopped),
        Arc::new(crate::subagent::tasks::Tasks::new()),
    );
    runner.unattended_turn_timeout = Some(Duration::from_secs(30 * 60));
    runner.headless = headless;
    runner.schedule_origin.set(Some(origin));
    let cancel = CancellationToken::new();
    let mut emit = |_| {};
    let run = runner.run("hello", &mut emit, &cancel);
    tokio::pin!(run);
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut run)
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    match tokio::time::timeout(Duration::from_secs(1), &mut run).await {
        Ok(result) => {
            assert!(result.is_err(), "{result:?}");
            true
        }
        Err(_) => false,
    }
}

#[tokio::test(start_paused = true)]
async fn chat_turn_ends_at_the_unattended_deadline() {
    let origin = otto_session::origin::Origin::chat("telegram", "1").unwrap();
    assert!(unattended_turn_ends(origin, false).await);
}

#[tokio::test(start_paused = true)]
async fn terminal_turn_ignores_the_unattended_deadline() {
    assert!(!unattended_turn_ends(otto_session::origin::Origin::terminal(), false).await);
}

#[tokio::test(start_paused = true)]
async fn headless_terminal_turn_ends_at_the_unattended_deadline() {
    assert!(unattended_turn_ends(otto_session::origin::Origin::terminal(), true).await);
}
