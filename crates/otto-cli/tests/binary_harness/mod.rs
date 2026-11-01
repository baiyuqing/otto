//! Helpers shared by the tests that drive the built `otto` binary: the PTY
//! screen scraper and the ACP tool-call assertion. The scripted provider they
//! talk to is `otto::fixtures::scripted_provider`.

// Each test binary compiles its own copy, so some helpers are unused in some.
#![allow(dead_code)]

#[cfg(unix)]
pub mod pty;

/// A displayed ACP tool call must close exactly once, including on failure.
pub fn assert_terminal_tool_update(frames: &[serde_json::Value], id: &str, status: &str) {
    let updates: Vec<_> = frames
        .iter()
        .filter(|frame| frame["method"] == "session/update")
        .map(|frame| &frame["params"]["update"])
        .filter(|update| update["toolCallId"] == id)
        .collect();
    assert_eq!(
        updates
            .iter()
            .filter(|update| update["sessionUpdate"] == "tool_call")
            .count(),
        1,
        "{updates:?}"
    );
    let terminal: Vec<_> = updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "tool_call_update")
        .collect();
    assert_eq!(terminal.len(), 1, "{updates:?}");
    assert_eq!(terminal[0]["status"], status, "{updates:?}");
}
