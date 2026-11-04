mod session_delete;

use super::*;
use crate::fixtures::{FakeSandbox, builder, controller, initial_runtime, seatbelt_info, user};
use crate::runtime::info::SandboxNetwork;
use crate::runtime::sandbox_config::SandboxChange;

/// The `heartbeat` file layout `failover::lease` writes, reproduced here
/// to simulate a running holder without a live `Lease` of this process's
/// own (see [`crate::session::tests`]'s test-safety note for the same
/// pattern).
fn lease_heartbeat_bytes(epoch: u64, host: &str, pid: u32, released: bool, seq: u64) -> Vec<u8> {
    let json = serde_json::json!({
        "epoch": epoch,
        "host": host,
        "pid": pid,
        "released": released,
        "seq": seq,
    })
    .to_string();
    let mut bytes = vec![b' '; 512];
    bytes[..json.len()].copy_from_slice(json.as_bytes());
    bytes[511] = b'\n';
    bytes
}

/// A child transcript at `<parent without .jsonl>/<task_id>-child.jsonl`,
/// matching the file `subagent::runner::spawn` creates. Duplicated from
/// [`crate::runtime::builder::tests::child_store`]: both are
/// `#[cfg(test)]`-private to their own module, and the fixture is 12
/// lines.
fn child_store(parent: &Path, task_id: &str) -> (crate::subagent::runner::Transcript, String) {
    let name = format!("{task_id}-child");
    let store = crate::session::Store::create_child_lazy(
        parent,
        &name,
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("child store");
    let path = crate::session::Store::child_path(parent, &name);
    (Arc::new(store), path.to_string_lossy().into_owned())
}

/// Reads a lease's fixed-size heartbeat file and reports whether its
/// `released` field is `true`. Parses only that one field, matching
/// [`crate::failover::lease`]'s own doc comment on why the heartbeat is
/// read back in tests elsewhere in this crate.
fn heartbeat_released(session_path: &str) -> bool {
    let heartbeat = Path::new(session_path)
        .with_extension("lease")
        .join("heartbeat");
    let bytes = std::fs::read(&heartbeat).expect("read heartbeat");
    let text = String::from_utf8_lossy(&bytes);
    text.contains("\"released\":true")
}

fn propose_pending(controller: &Controller, key: &str) {
    let memory = controller.memory_wiring();
    memory
        .service
        .propose(&crate::memory::ProposeRequest {
            scope: controller.workspace_memory_scope().clone(),
            kind: "preference".into(),
            key: key.into(),
            text: key.into(),
            reason: "said so".into(),
            confidence: 0.5,
            source: crate::memory::Provenance {
                origin: Some(crate::memory::Origin::Model),
                session_id: controller.info().session_id,
                ..crate::memory::Provenance::default()
            },
            ..crate::memory::ProposeRequest::default()
        })
        .expect("propose");
}

mod admission;
mod approvals;
mod memory_notice;
mod migrate;
mod permissions_command;
mod resume_and_archive;
mod sandbox_amendment;
mod session_creation;
mod session_owned_resources;
mod session_replacement;
mod tasks_and_wake;
