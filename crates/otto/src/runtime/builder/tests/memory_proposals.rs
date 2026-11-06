use super::*;

#[tokio::test]
async fn runner_with_a_usable_native_memory_store_has_a_proposal_generator() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut builder = builder(dir.path());
    let memory_runtime = otto_core::config::MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: dir.path().join("memory").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let (service, user_scope, usable) =
        crate::runtime::wiring::open_memory_service(&memory_runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the test store must be usable");
    builder.shared_mut().memory.service = service;
    builder.shared_mut().memory.user_scope = user_scope;
    builder.shared_mut().memory.usable = usable;
    builder.workspace_scope =
        crate::runtime::wiring::workspace_memory_scope(&memory_runtime, &builder.workspace_path)
            .expect("workspace scope");
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    assert!(runner.memory_proposals.is_some());
}

#[tokio::test]
async fn runner_without_usable_memory_has_no_proposal_generator() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    assert!(!builder.memory_usable());
    let session = SharedSession::memory(Header::default());
    let runner = builder
        .build_runner(&session, &runtime())
        .await
        .expect("runner");
    assert!(runner.memory_proposals.is_none());
}
