use super::*;
use crate::decision::{Answer, Exchange};
use std::{collections::HashMap, sync::Mutex};

pub(crate) struct Fixed(pub Result<(&'static str, Option<f64>), Failure>);
#[async_trait::async_trait]
impl Backend for Fixed {
    fn model(&self) -> String {
        "private-model".into()
    }
    fn auxiliary(&self) -> bool {
        true
    }
    async fn ask(&self, _: &str, _: &[Question], _: Duration) -> Exchange {
        Exchange {
            request: "private request".into(),
            response: Some("private response".into()),
            result: self.0.clone().map(|(choice, probability)| {
                HashMap::from([(
                    QUESTION.into(),
                    Answer {
                        choice: choice.into(),
                        probability,
                    },
                )])
            }),
        }
    }
}
fn gate(result: Result<(&'static str, Option<f64>), Failure>) -> Gate {
    Gate::new(
        Arc::new(Fixed(result)),
        Arc::new(crate::log::Logger::discard()),
    )
}

#[tokio::test]
async fn only_valid_defer_can_defer_and_confidence_never_authorizes() {
    for p in [None, Some(0.0), Some(1.0), Some(0.3)] {
        assert_eq!(
            gate(Ok(("defer", p)))
                .decide("evidence", "on_work", &CancellationToken::new())
                .await,
            Outcome::Defer
        );
        assert_eq!(
            gate(Ok(("immediate", p)))
                .decide("evidence", "on_work", &CancellationToken::new())
                .await,
            Outcome::Immediate
        );
    }
    for p in [Some(-0.1), Some(1.1), Some(f64::NAN), Some(f64::INFINITY)] {
        assert_eq!(
            gate(Ok(("defer", p)))
                .decide("evidence", "on_work", &CancellationToken::new())
                .await,
            Outcome::Immediate
        );
    }
    for result in [
        Ok(("grant", None)),
        Err(Failure::Timeout),
        Err(Failure::Error("secret".into())),
        Err(Failure::Invalid("private".into())),
    ] {
        assert_eq!(
            gate(result)
                .decide("evidence", "on_work", &CancellationToken::new())
                .await,
            Outcome::Immediate
        );
    }
}

struct Pending;
#[async_trait::async_trait]
impl Backend for Pending {
    fn model(&self) -> String {
        "pending".into()
    }
    fn auxiliary(&self) -> bool {
        true
    }
    async fn ask(&self, _: &str, _: &[Question], _: Duration) -> Exchange {
        std::future::pending().await
    }
}
#[tokio::test]
async fn outer_deadline_fails_open_and_cancellation_wins() {
    let mut gate = Gate::new(Arc::new(Pending), Arc::new(crate::log::Logger::discard()));
    gate.limit = Duration::from_millis(1);
    assert_eq!(
        gate.decide("", "on_work", &CancellationToken::new()).await,
        Outcome::Immediate
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        gate.decide("", "on_work", &cancel).await,
        Outcome::Cancelled
    );
    gate.limit = Duration::from_secs(60);
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let (_, result) = tokio::join!(
        async move {
            tokio::task::yield_now().await;
            child.cancel();
        },
        gate.decide("", "on_work", &cancel)
    );
    assert_eq!(result, Outcome::Cancelled);
}

struct Sink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn structured_event_excludes_state_exchange_and_backend_errors() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Gate::new(
        Arc::new(Fixed(Err(Failure::Error("private-error".into())))),
        Arc::new(crate::log::Logger::new(Box::new(Sink(bytes.clone())))),
    );
    gate.decide("private-evidence", "on_work", &CancellationToken::new())
        .await;
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(text.contains("reflection_decision"));
    assert!(text.contains("unavailable"));
    assert!(text.contains("latency_ms"));
    assert!(!text.contains("private"), "{text}");
}
