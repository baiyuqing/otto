//! Session execution authority is separate from retained turn replay evidence.

use super::*;

fn activity_turn(id: &str) -> Arc<Turn> {
    Arc::new(Turn::new(
        id.to_string(),
        turn::TRIGGER_USER,
        CancellationToken::new(),
    ))
}

fn queued_activity_turn(id: &str) -> QueuedTurn {
    QueuedTurn {
        input_provenance: None,
        turn: Arc::new(Turn::queued(
            id.to_string(),
            turn::TRIGGER_USER,
            CancellationToken::new(),
        )),
        text: id.to_string(),
        image: None,
        channel: None,
        origin: None,
        input_id: None,
    }
}

#[test]
fn a_turn_holds_activity_until_settlement_even_after_cancellation() {
    let mut state = SessionState::default();
    let first = activity_turn("first");
    assert!(state.begin_turn(Arc::clone(&first)));
    first.cancel();
    assert!(first.cancel_token().is_cancelled());
    assert!(state.turn_running());
    assert!(!state.begin_turn(activity_turn("second")));
    assert!(state.begin_compaction(CancellationToken::new()).is_none());

    // A completed replay record alone does not release execution authority.
    first.finish(None, false);
    assert!(state.busy());
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
    assert!(state.finish_turn(&first));
    assert!(!state.busy());
    assert!(Arc::ptr_eq(
        state.last_turn().expect("retained turn"),
        &first
    ));
    let compaction = state
        .begin_compaction(CancellationToken::new())
        .expect("settled turn releases activity");
    assert!(state.is_compacting());
    assert!(!state.begin_turn(activity_turn("second")));
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
    assert!(state.finish_compaction(compaction));
}

#[test]
fn stale_turn_completion_cannot_release_a_new_turn_or_compaction() {
    let mut state = SessionState::default();
    let first = activity_turn("same-id");
    assert!(state.begin_turn(Arc::clone(&first)));
    // Identity, rather than a caller-supplied ID, decides ownership.
    assert!(!state.finish_turn(&activity_turn("same-id")));
    assert!(state.turn_running());
    first.finish(None, false);
    assert!(state.finish_turn(&first));
    let second = activity_turn("second");
    assert!(state.begin_turn(Arc::clone(&second)));
    assert!(!state.finish_turn(&first));
    assert!(state.turn_running());
    assert!(Arc::ptr_eq(state.last_turn().expect("latest"), &second));
    second.finish(None, false);
    assert!(state.finish_turn(&second));
    let compaction = state
        .begin_compaction(CancellationToken::new())
        .expect("idle");
    assert!(!state.finish_turn(&second));
    assert!(state.is_compacting());
    assert!(state.finish_compaction(compaction));
}

#[test]
fn stale_compaction_completion_cannot_release_new_activity() {
    let mut state = SessionState::default();
    let first = state
        .begin_compaction(CancellationToken::new())
        .expect("first compaction");
    assert!(state.finish_compaction(first));
    let second = state
        .begin_compaction(CancellationToken::new())
        .expect("second compaction");
    assert_ne!(first, second);
    assert!(!state.finish_compaction(first));
    assert!(state.is_compacting());
    assert!(!state.begin_turn(activity_turn("blocked")));
    assert!(state.finish_compaction(second));
    let turn = activity_turn("next");
    assert!(state.begin_turn(Arc::clone(&turn)));
    assert!(!state.finish_compaction(second));
    assert!(state.turn_running());
    turn.finish(None, false);
    assert!(state.finish_turn(&turn));
}

#[test]
fn closing_during_a_turn_never_reopens_admission() {
    let mut state = SessionState::default();
    let turn = activity_turn("active");
    assert!(state.begin_turn(Arc::clone(&turn)));
    state.begin_close();
    assert!(state.is_closing());
    assert!(state.turn_running(), "closing retains active ownership");
    assert!(!state.begin_turn(activity_turn("blocked")));
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
    turn.finish(Some(String::new()), true);
    assert!(state.finish_turn(&turn));
    state.begin_close();
    assert!(state.is_closing());
    assert!(!state.begin_turn(activity_turn("still-blocked")));
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
    assert!(Arc::ptr_eq(state.last_turn().expect("replay"), &turn));
}

#[test]
fn closing_during_compaction_never_reopens_admission() {
    let mut state = SessionState::default();
    let compaction = state
        .begin_compaction(CancellationToken::new())
        .expect("idle");
    state.begin_close();
    assert!(state.is_closing());
    assert!(state.is_compacting(), "closing retains active ownership");
    assert!(!state.begin_turn(activity_turn("blocked")));
    assert!(state.finish_compaction(compaction));
    assert!(state.is_closing());
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
    assert!(!state.begin_turn(activity_turn("still-blocked")));
}

#[test]
fn compaction_cannot_overtake_queue_or_restored_inputs_but_recovery_can() {
    let mut state = SessionState::default();
    state.queue.push_back(queued_activity_turn("queued"));
    assert!(state.busy_for_wake());
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
    let queued = state.queue.pop_front().expect("queued");
    state.restored.push_back(queued);
    assert!(state.busy());
    assert!(
        !state.busy_for_wake(),
        "recovery must precede restored inputs"
    );
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
    let recovery = Arc::new(Turn::new(
        "recovery".to_string(),
        turn::TRIGGER_RECOVERY,
        CancellationToken::new(),
    ));
    assert!(state.begin_turn(Arc::clone(&recovery)));
    recovery.finish(None, false);
    assert!(state.finish_turn(&recovery));
    assert!(
        state.busy(),
        "restored work still blocks ordinary admission"
    );
    assert!(state.begin_compaction(CancellationToken::new()).is_none());
}

#[tokio::test]
async fn close_cancels_active_work_and_seals_a_stale_session_handle() {
    let (gate, options) = gated();
    let harness = Harness::with(options);
    let id = harness.create().await;
    let response = post_turn(&harness, &id, "active", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(5), harness.provider.wait_started(1))
        .await
        .expect("active provider call");
    let session = harness.server.lookup(&id).expect("session");
    let turn = session.current_turn().expect("active turn");
    let closed = harness
        .send("DELETE", &format!("/v1/sessions/{id}"), None)
        .await;
    assert_eq!(closed.status, StatusCode::NO_CONTENT, "{}", closed.body);
    assert!(turn.cancel_token().is_cancelled());
    assert!(session.lock().is_closing());
    assert!(harness.server.lookup(&id).is_none());
    ends_of(SseReader::new(response)).await;
    assert!(
        session.lock().is_closing(),
        "settlement must not reopen a stale handle"
    );
    for queue in [false, true] {
        assert!(
            harness
                .server
                .start_turn(
                    &session,
                    "stale".to_string(),
                    None,
                    (None, None),
                    queue,
                    "",
                    None,
                )
                .is_err()
        );
    }
    assert!(
        session
            .lock()
            .begin_compaction(CancellationToken::new())
            .is_none()
    );
    assert_eq!(harness.provider.texts(), ["active"]);
    gate.cancel();
}

#[tokio::test]
async fn compaction_retains_completed_status_and_replay_while_blocking_new_activity() {
    const TURNS: usize = 3;
    let summary_text = otto_core::agent::summary_validate::REQUIRED_SUMMARY_HEADINGS
        .iter()
        .map(|heading| format!("{heading}\nNone.\n"))
        .collect::<Vec<_>>()
        .join("\n");
    let gate = CancellationToken::new();
    let harness = Harness::with(HarnessOptions {
        script: Script {
            deltas: vec![summary_text],
            gate: Some(gate.clone()),
            gate_after_calls: TURNS,
            ..Script::default()
        },
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    for _ in 0..TURNS {
        super::sessions_cases::complete_turn(&harness, &id, &"x".repeat(80_000)).await;
    }
    let session = harness.server.lookup(&id).expect("session");
    let completed = session.current_turn().expect("completed turn");
    let path = format!("/v1/sessions/{id}/compact");
    let mut compaction = Box::pin(harness.send("POST", &path, None));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            reply = &mut compaction => panic!("compaction passed gate: {}", reply.body),
            () = harness.provider.wait_started(TURNS + 1) => {}
        }
    })
    .await
    .expect("compaction provider started");
    assert!(session.lock().is_compacting());
    let summary = harness
        .send(
            "GET",
            &format!("/v1/sessions/{id}/turns/{}", completed.id),
            None,
        )
        .await;
    assert_eq!(summary.status, StatusCode::OK);
    assert_eq!(summary.json()["status"], turn::TURN_OK);
    assert!(Arc::ptr_eq(
        &session.current_turn().expect("retained"),
        &completed
    ));
    let mut status = SseReader::new(harness.raw("GET", "/v1/status", None, &[]).await);
    assert_eq!(
        status_snapshot(&mut status).await["sessions"][0]["turn"],
        turn::TURN_OK
    );
    let replay = harness
        .raw(
            "GET",
            &format!("/v1/sessions/{id}/turns/{}/events", completed.id),
            None,
            &[],
        )
        .await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(ends_of(SseReader::new(replay)).await.2, turn::TURN_OK);
    assert_eq!(
        post_turn(&harness, &id, "blocked", false).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        harness.send("POST", &path, None).await.status,
        StatusCode::CONFLICT
    );
    let retained_replay = harness
        .raw(
            "GET",
            &format!("/v1/sessions/{id}/turns/{}/events", completed.id),
            None,
            &[],
        )
        .await;
    assert_eq!(retained_replay.status(), StatusCode::OK);
    let queued = post_turn(&harness, &id, "queued-behind-compaction", true).await;
    assert_eq!(queued.status(), StatusCode::OK);
    assert_eq!(
        harness.provider.texts().len(),
        TURNS + 1,
        "queued turn cannot enter the provider yet"
    );
    gate.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), compaction)
        .await
        .expect("compaction ends");
    assert_eq!(result.status, StatusCode::OK, "{}", result.body);
    assert!(!session.lock().is_compacting());
    assert_eq!(ends_of(SseReader::new(queued)).await.2, turn::TURN_OK);
    assert_eq!(
        harness.provider.texts().last().map(String::as_str),
        Some("queued-behind-compaction")
    );
    let next = post_turn(&harness, &id, "next", false).await;
    assert_eq!(next.status(), StatusCode::OK);
    assert_eq!(
        ends_of(SseReader::new(retained_replay)).await.2,
        turn::TURN_OK
    );
    assert_eq!(ends_of(SseReader::new(next)).await.2, turn::TURN_OK);
    assert_eq!(completed.summary().status, turn::TURN_OK);
    assert!(
        completed.snapshot(0).1,
        "retained readers stay completed after a new turn"
    );
}

#[tokio::test]
async fn cancel_work_keeps_compaction_owned_and_seals_its_stale_handle() {
    let harness = Harness::new();
    let id = harness.create().await;
    let session = harness.server.lookup(&id).expect("session");
    let cancel = CancellationToken::new();
    let compaction = session
        .lock()
        .begin_compaction(cancel.clone())
        .expect("idle");
    session.cancel_work();
    assert!(
        cancel.is_cancelled(),
        "close signals compaction cancellation"
    );
    assert!(
        session.lock().is_compacting(),
        "cancellation retains ownership until settlement"
    );
    assert!(session.lock().is_closing());
    assert!(session.lock().finish_compaction(compaction));
    assert!(
        session.lock().is_closing(),
        "compaction settlement must preserve Closing"
    );
    assert!(
        session
            .lock()
            .begin_compaction(CancellationToken::new())
            .is_none()
    );
    assert!(
        harness
            .server
            .start_turn(
                &session,
                "stale".to_string(),
                None,
                (None, None),
                false,
                "",
                None,
            )
            .is_err()
    );
    assert!(harness.provider.texts().is_empty());
}
