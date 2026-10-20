use super::*;

struct PendingProvider {
    polls: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log bytes").extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Provider for PendingProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let _drop = DropFlag(Arc::clone(&self.dropped));
        control.cancellation_token().cancelled().await;
        std::future::pending().await
    }
}

#[tokio::test]
async fn remote_compaction_logs_safe_diagnostic_metadata_without_opaque_content() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let logger = crate::log::Logger::new(Box::new(LogSink(Arc::clone(&bytes))));
    let session = SharedSession::memory(Header::default());
    session
        .append_remote_compaction(otto_core::session::RemoteCompaction {
            provider: "chatgpt".into(),
            id: "compact-1".into(),
            encrypted_content: "opaque-secret-must-not-appear".into(),
        })
        .await
        .expect("append checkpoint");

    log_runtime_event(
        &logger,
        &session,
        &Event::ProviderApiCall {
            operation_id: otto_core::model::OperationId::new("operation-1").unwrap(),
            kind: otto_core::agent::ProviderOperationKind::RemoteCompaction,
            provider: "chatgpt".into(),
            model: "test-model".into(),
            duration: Duration::from_millis(42),
            attempts: 2,
            status: otto_core::agent::ApiStatus::Ok,
            outcome: otto_core::model::OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        },
    );
    log_runtime_event(
        &logger,
        &session,
        &Event::CompactionCompleted {
            compaction: CompactionResult {
                checkpoint_id: "compact-1".into(),
                tokens_before: Some(120),
                usage_present: true,
                cached_input_tokens_present: true,
                ..CompactionResult::default()
            },
        },
    );

    let text = String::from_utf8(bytes.lock().expect("log bytes").clone()).unwrap();
    for expected in [
        "msg=provider_operation_finished",
        "operation_id=operation-1",
        "kind=remote_compaction",
        "attempts=2",
        "status=ok",
        "effect_certainty=completed",
        "duration_ms=42",
        "msg=remote_compaction_finished",
        "checkpoint_id=compact-1",
        "provider=chatgpt",
        "tail_messages=0",
        "tokens_before=120",
        "usage_present=true",
        "cached_input_tokens_present=true",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.contains("opaque-secret-must-not-appear"), "{text}");
}

#[tokio::test]
async fn a_noop_compaction_does_not_log_an_unnamed_remote_checkpoint() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let logger = crate::log::Logger::new(Box::new(LogSink(Arc::clone(&bytes))));
    let session = SharedSession::memory(Header::default());
    session
        .append_remote_compaction(otto_core::session::RemoteCompaction {
            provider: "chatgpt".into(),
            id: String::new(),
            encrypted_content: "opaque".into(),
        })
        .await
        .expect("append checkpoint");

    log_runtime_event(
        &logger,
        &session,
        &Event::CompactionCompleted {
            compaction: CompactionResult {
                noop: true,
                ..CompactionResult::default()
            },
        },
    );

    let text = String::from_utf8(bytes.lock().expect("log bytes").clone()).unwrap();
    assert!(!text.contains("remote_compaction_finished"), "{text}");
}

#[tokio::test(start_paused = true)]
async fn provider_deadline_drops_an_uncooperative_future_after_grace() {
    let polls = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = PendingProvider {
        polls: Arc::clone(&polls),
        dropped: Arc::clone(&dropped),
    };
    let parent = Control::new(Deadline::unlimited());
    let mut emit = |_| {};
    let request = Request::default();
    let complete = complete_with_timeout(
        &provider,
        &request,
        &mut emit,
        &parent,
        Some(Duration::from_secs(2)),
        Duration::from_secs(3),
    );
    tokio::pin!(complete);
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut complete)
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut complete)
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(3)).await;
    let settlement = complete.await;

    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(settlement.attempts, 1);
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::DeadlineExceeded
    );
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
}

#[tokio::test(start_paused = true)]
async fn expired_parent_refuses_provider_before_polling_it() {
    let polls = Arc::new(AtomicUsize::new(0));
    let provider = PendingProvider {
        polls: Arc::clone(&polls),
        dropped: Arc::new(AtomicBool::new(false)),
    };
    let parent = Control::new(Deadline::after(Duration::ZERO));
    let mut emit = |_| {};
    let request = Request::default();
    let settlement = complete_with_timeout(
        &provider,
        &request,
        &mut emit,
        &parent,
        None,
        Duration::from_secs(3),
    )
    .await;

    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(settlement.attempts, 0);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
    assert_eq!(
        settlement.outcome.disposition,
        OperationDisposition::DeadlineExceeded
    );
}

#[tokio::test(start_paused = true)]
async fn drive_prefers_an_already_ready_completion() {
    let parent = CancellationToken::new();
    parent.cancel();
    let control = Control::new(Deadline::after(Duration::ZERO));

    let result = drive_with_control(async { 42 }, &parent, &control).await;

    assert_eq!(result, 42);
    assert_eq!(control.stop_reason(), None);
}

#[tokio::test(start_paused = true)]
async fn deadline_waits_for_the_owned_future_to_clean_up() {
    let parent = CancellationToken::new();
    let control = Control::new(Deadline::after(Duration::from_secs(5)));
    let cleaned = AtomicBool::new(false);
    let run = async {
        control.cancellation_token().cancelled().await;
        cleaned.store(true, Ordering::SeqCst);
    };

    let drive = drive_with_control(run, &parent, &control);
    tokio::pin!(drive);
    tokio::time::advance(Duration::from_secs(5)).await;
    drive.await;

    assert!(cleaned.load(Ordering::SeqCst));
    assert_eq!(control.stop_reason(), Some(OperationStopReason::Deadline));
}

#[tokio::test]
async fn parent_cancellation_keeps_its_typed_reason_and_waits_for_cleanup() {
    let parent = CancellationToken::new();
    let control = Control::new(Deadline::unlimited());
    let cleaned = AtomicBool::new(false);
    let run = async {
        control.cancellation_token().cancelled().await;
        cleaned.store(true, Ordering::SeqCst);
    };
    parent.cancel();

    drive_with_control(run, &parent, &control).await;

    assert!(cleaned.load(Ordering::SeqCst));
    assert_eq!(
        control.stop_reason(),
        Some(OperationStopReason::UserCancellation)
    );
}
