//! Runs the `otto_core::conformance` session checks against the POSIX
//! `Store` and the public wrappers built on it. The private wrappers
//! (`SharedTranscript`, the sidecar mailbox adapter) run them in their own
//! module tests.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use otto::runtime::builder::{MemoryHandle, SharedSession};
use otto_core::conformance;
use otto_core::session::Header;
use otto_session::Store;

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn header(workspace: &Path) -> Header {
    Header {
        id: format!("conformance-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        workspace: workspace.to_string_lossy().into_owned(),
        provider: "openai-compatible".into(),
        model: "test-model".into(),
        created_at: chrono::Utc::now(),
        ..Header::default()
    }
}

fn reopen_store(store: Store) -> Store {
    let path = store.path();
    store.close().expect("close");
    drop(store);
    Store::open(&path).expect("reopen").0
}

#[tokio::test]
async fn the_posix_store_keeps_the_session_contract_across_a_reopen() {
    let root = tempfile::tempdir().expect("root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fresh = || Store::create(root.path(), header(workspace.path())).expect("create");
    conformance::session(&fresh, Some(&reopen_store))
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}

#[tokio::test]
async fn a_shared_session_over_a_store_keeps_the_session_contract_across_a_reopen() {
    let root = tempfile::tempdir().expect("root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fresh = || {
        let store = Store::create(root.path(), header(workspace.path())).expect("create");
        SharedSession::new(Arc::new(store))
    };
    let reopen = |session: SharedSession| {
        let path = session.path();
        session.close().expect("close");
        drop(session);
        SharedSession::new(Arc::new(Store::open(&path).expect("reopen").0))
    };
    conformance::session(&fresh, Some(&reopen))
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}

#[tokio::test]
async fn a_memory_shared_session_keeps_the_session_contract() {
    let workspace = tempfile::tempdir().expect("workspace");
    let fresh = || SharedSession::memory(header(workspace.path()));
    conformance::session(&fresh, None)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}

#[tokio::test]
async fn a_memory_handle_keeps_the_session_contract() {
    let workspace = tempfile::tempdir().expect("workspace");
    let fresh = || MemoryHandle::new(header(workspace.path()));
    conformance::session(&fresh, None)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
}
