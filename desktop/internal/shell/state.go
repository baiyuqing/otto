// Package shell owns the desktop's process and HTTP contracts, independent
// of the native window toolkit. The UI remains a client of otto serve.
package shell

import (
	"encoding/json"
	"os"
	"path/filepath"
)

const BundleID = "com.otto.desktop"

type State struct {
	Workspace string `json:"workspace"`
}

// Locations deliberately retain Tauri's paths, rather than MyGo's default
// app-name-based directories, so upgrading retains the selected workspace.
func Locations(home string) (state, log string) {
	return filepath.Join(home, "Library", "Application Support", BundleID, "state.json"),
		filepath.Join(home, "Library", "Logs", BundleID, "serve.log")
}

func LoadState(path string) State {
	var state State
	if data, err := os.ReadFile(path); err == nil {
		if json.Unmarshal(data, &state) != nil {
			return State{}
		}
	}
	return state
}

func SaveState(path string, state State) error {
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0600)
}
