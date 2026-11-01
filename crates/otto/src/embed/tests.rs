use super::*;
use crate::fixtures::embed_harness::*;
use otto_core::{agent::Event, model::BlockType};

/// `build()` is exclusive per process, so every test that builds an
/// `Otto` holds this for its whole body.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The tool names of a new session, with `disabled` switched off, and the
/// entry names under the test's `~/.otto` after the runtime closed.
async fn observe(fixture: &Fixture, disabled: &[Component]) -> (Vec<String>, Vec<String>) {
    let mut options = builder(fixture);
    for component in disabled {
        options = options.disable(*component);
    }
    let otto = options.build().await.expect("build");
    let session = otto.new_session(None).await.expect("session");
    let tools = session.controller.tool_names();
    drop(session);
    otto.close().await.expect("close");
    let mut entries: Vec<String> = std::fs::read_dir(fixture.home.join(".otto"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    (tools, entries)
}

fn has(names: &[String], wanted: &[&str]) -> bool {
    wanted.iter().all(|name| names.iter().any(|n| n == name))
}

fn lacks(names: &[String], unwanted: &[&str]) -> bool {
    unwanted.iter().all(|name| !names.iter().any(|n| n == name))
}

/// Runs the switch for `component` against a control without it:
/// `tools` appear in the control and not with the switch; `files` are
/// created by the control and not with the switch.
async fn switch(component: Component, extra: &str, tools: &[&str], files: &[&str]) {
    let _serial = LOCK.lock().await;
    let (control_tools, control_files) = observe(&fixture(extra), &[]).await;
    assert!(
        has(&control_tools, tools),
        "control tools {control_tools:?}"
    );
    for file in files {
        assert!(
            control_files.iter().any(|f| f == file),
            "control files {control_files:?} lack {file}"
        );
    }
    let (tools_off, files_off) = observe(&fixture(extra), &[component]).await;
    assert!(
        lacks(&tools_off, tools),
        "{component:?} tools {tools_off:?}"
    );
    for file in files {
        assert!(
            !files_off.iter().any(|f| f == file),
            "{component:?} files {files_off:?} hold {file}"
        );
    }
}

mod disabling;
mod lifecycle;
