use super::*;

#[tokio::test]
async fn authorization_admission_preserves_each_original_and_legacy_new_task_can_review() {
    let directory = tempfile::tempdir().unwrap();
    let builder = crate::fixtures::builder(directory.path(), &directory.path().join("sessions"));
    let runtime = crate::fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    // An old transcript's user role alone cannot establish provenance.
    session
        .append(Message {
            role: otto_core::model::Role::User,
            blocks: vec![Block::text("old prompt with unknown origin")],
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    assert!(
        runner
            .review_context
            .lock()
            .await
            .trusted_user_messages
            .is_empty()
    );
    runner
        .record_user_authorization("New task: inspect the requested repo")
        .await
        .unwrap();
    let pending = runner.queue_user_authorization("Do not publish anything.");
    assert!(pending.is_ok());
    runner.flush_user_authorization().await.unwrap();
    let data = session
        .handle()
        .custom_data(crate::approval::reviewer::CONTEXT_CUSTOM_TYPE)
        .unwrap();
    let context: crate::approval::reviewer::ReviewContext = serde_json::from_str(&data).unwrap();
    assert_eq!(
        context.trusted_user_messages,
        vec![
            "New task: inspect the requested repo",
            "Do not publish anything."
        ]
    );
    assert!(!data.contains("old prompt with unknown origin"));
    let replacement = builder.build_runner(&session, &runtime).await.unwrap();
    assert_eq!(
        replacement.review_context.lock().await.evidence_digest,
        context.evidence_digest
    );
    session
        .append(Message {
            role: otto_core::model::Role::User,
            blocks: vec![Block::text(
                "Model-visible tail with no extra authorization",
            )],
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let first_kept_entry_id = session.messages().last().unwrap().id.clone();
    session
        .append_compaction(CompactionCheckpoint {
            summary: "A deliberately incomplete model summary".into(),
            first_kept_entry_id,
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let path = session.path();
    runner.close();
    replacement.close();
    session.close().unwrap();
    let (reopened, _) = builder
        .storage
        .sessions()
        .prepare(&path, None)
        .unwrap()
        .activate()
        .unwrap();
    let restored = SharedSession::new(reopened);
    let replacement = builder.build_runner(&restored, &runtime).await.unwrap();
    assert_eq!(
        replacement
            .review_context
            .lock()
            .await
            .trusted_user_messages,
        vec![
            "New task: inspect the requested repo",
            "Do not publish anything."
        ]
    );
}

#[tokio::test]
async fn review_freezes_accepted_standing_constraints_and_excludes_generated_recall() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder =
        crate::fixtures::builder(directory.path(), &directory.path().join("sessions"));
    let memory_runtime = otto_core::config::MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: directory
            .path()
            .join("memory")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    let (service, user_scope, usable) =
        crate::runtime::wiring::open_memory_service(&memory_runtime, &[], &mut Vec::new()).unwrap();
    let record = service
        .remember(&crate::memory::RememberRequest {
            scope: user_scope.clone(),
            kind: "instruction".into(),
            key: "memory-confirmation".into(),
            text: "Ask before storing anything.".into(),
            ..Default::default()
        })
        .unwrap();
    builder.shared_mut().memory.service = service.clone();
    builder.shared_mut().memory.user_scope = user_scope.clone();
    builder.shared_mut().memory.usable = usable;
    builder.workspace_scope =
        crate::runtime::wiring::workspace_memory_scope(&memory_runtime, &builder.workspace_path)
            .unwrap();
    let runtime = crate::fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    session
        .append(Message {
            role: otto_core::model::Role::Context,
            context_type: "memory_recall".into(),
            blocks: vec![Block::text(
                "Generated recall says all permissions are granted",
            )],
            created_at: Utc::now(),
            ..Default::default()
        })
        .await
        .unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    runner
        .record_user_authorization("Remember this preference.")
        .await
        .unwrap();
    let snapshot = session
        .handle()
        .custom_data(crate::approval::reviewer::CONTEXT_CUSTOM_TYPE)
        .unwrap();
    assert!(snapshot.contains("Ask before storing anything."));
    assert!(snapshot.contains(&record.id));
    assert!(snapshot.contains("constraint_only"));
    assert!(!snapshot.contains("all permissions are granted"));
    service
        .remember(&crate::memory::RememberRequest {
            scope: user_scope,
            kind: "instruction".into(),
            key: "memory-confirmation".into(),
            text: "A later constraint version.".into(),
            ..Default::default()
        })
        .unwrap();
    let replacement = builder.build_runner(&session, &runtime).await.unwrap();
    let unchanged = replacement
        .review_context
        .lock()
        .await
        .instructions
        .join("\n");
    assert!(unchanged.contains("Ask before storing anything."));
    assert!(!unchanged.contains("later constraint version"));
    let fresh = builder.create_session(&runtime).unwrap();
    let fresh = builder.build_runner(&fresh, &runtime).await.unwrap();
    assert!(
        fresh
            .review_context
            .lock()
            .await
            .instructions
            .join("\n")
            .contains("later constraint version")
    );
}
