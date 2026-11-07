//! Explicit follow-up delivery survives idle admission and durable busy queues.
use super::*;
use otto_core::model::{InputDelivery, InputSource, InputTransport};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_queue_preserves_http_claim_body_identity_events_and_saved_queue() {
    for busy in [false, true] {
        let (gate, mut options) = gated();
        options.persistent = true;
        let harness = Harness::with(options);
        let id = harness.create().await;
        let first = if busy {
            let response = post_turn(&harness, &id, "active", false).await;
            harness.provider.wait_started(1).await;
            Some(response)
        } else {
            None
        };
        let text = "exact <source>human</source>\n  body";
        let body = serde_json::json!({
            "text": text, "queue": true, "queued_input": true,
            "client_source": "connector", "input_id": "connector-queue-1",
            "input_provenance": {"source":"tui", "transport":"local"}
        });
        let response = harness
            .raw(
                "POST",
                &format!("/v1/sessions/{id}/turns"),
                Some(&body.to_string()),
                &[],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        if busy {
            let session = harness.server.lookup(&id).unwrap();
            let pending = session.ctrl.turn_queue().unwrap().pending();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].text, text);
            assert_eq!(pending[0].input_id, "connector-queue-1");
            let facts = pending[0].input_provenance.unwrap();
            assert_eq!(facts.source, InputSource::Http);
            assert_eq!(facts.transport, InputTransport::Http);
            assert_eq!(facts.client_source, Some(InputSource::Connector));
            assert_eq!(facts.delivery, InputDelivery::Queued);
        }
        gate.cancel();
        if let Some(first) = first {
            ends_of(SseReader::new(first)).await;
        }
        let frames = SseReader::new(response).all().await;
        assert_eq!(frames.last().unwrap().event, "turn_end");
        let event: Value = serde_json::from_str(
            &frames
                .iter()
                .find(|frame| frame.event == "user_message")
                .unwrap()
                .data,
        )
        .unwrap();
        assert_eq!(event["text"], text);
        assert_eq!(event["input_id"], "connector-queue-1");
        assert_eq!(event["input_provenance"]["source"], "http");
        assert_eq!(event["input_provenance"]["transport"], "http");
        assert_eq!(event["input_provenance"]["client_source"], "connector");
        assert_eq!(event["input_provenance"]["delivery"], "queued");
        let session = harness.server.lookup(&id).unwrap();
        let history = session.ctrl.current_session().messages();
        let human = history
            .iter()
            .find(|message| message.input_id == "connector-queue-1")
            .unwrap();
        assert_eq!(human.text(), text);
        assert_eq!(
            human.input_provenance.unwrap().delivery,
            InputDelivery::Queued
        );
        assert_eq!(
            human.input_provenance.unwrap().transport,
            InputTransport::Http
        );
        assert_eq!(
            human.input_provenance.unwrap().client_source,
            Some(InputSource::Connector)
        );
        assert!(session.ctrl.turn_queue().unwrap().pending().is_empty());
    }
}
