//! `~/Library/Application Support/<bundle id>/state.json`: the one thing the
//! app remembers between launches, the last workspace directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub workspace: Option<String>,
}

pub fn state_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("state.json")
}

/// Reads `path`. A missing or unparsable file reads as the default (no
/// saved workspace), so a corrupt state file behaves like first launch
/// instead of failing startup.
pub fn load_state(path: &Path) -> State {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_state(path: &Path, state: &State) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state).expect("State always serializes");
    std::fs::write(path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_file() {
        let dir =
            std::env::temp_dir().join(format!("otto-desktop-state-test-{}", std::process::id()));
        let path = state_path(&dir);
        let state = State {
            workspace: Some("/Users/me/project".to_string()),
        };
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path), state);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_reads_as_default() {
        let path = Path::new("/nonexistent/otto-desktop/state.json");
        assert_eq!(load_state(path), State::default());
    }
}
