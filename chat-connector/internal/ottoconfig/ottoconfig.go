// Package ottoconfig reads the part of Otto's local user configuration that
// the connector displays in its model picker.
//
// It reads one file, ~/.config/otto/config.toml, and returns only model
// selectors. It never returns credentials: Otto's configuration stores the
// names of API-key environment variables, never their values, so nothing
// secret can be read here. Otto itself remains the writer of this file.
package ottoconfig

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"github.com/BurntSushi/toml"
)

// Model is one configured model, selected by "<Provider>/<ModelID>".
type Model struct {
	Selector string
	Provider string
	ModelID  string
}

// Snapshot is the local user configuration the picker needs.
type Snapshot struct {
	// Default is the selector of default_model; empty when unset.
	Default string
	// Models is sorted by selector, without duplicates.
	Models []Model
}

// Path is the user configuration source Otto reads by default.
func Path(home string) string {
	return filepath.Join(home, ".config", "otto", "config.toml")
}

// providerTable is the decoded subset of a [providers.<name>] table. Model
// tables are decoded as free maps: only their keys are needed.
type providerTable struct {
	Models map[string]map[string]any `toml:"models"`
}

// file declares only the decoded subset. Unknown keys and sections are
// ignored, so a newer Otto configuration still loads.
type file struct {
	DefaultModel string                   `toml:"default_model"`
	Providers    map[string]providerTable `toml:"providers"`
}

// Read parses path. A missing file is an empty snapshot rather than an error:
// Otto may be configured entirely through the workspace or managed layers.
func Read(path string) (Snapshot, error) {
	raw, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return Snapshot{}, nil
	}
	if err != nil {
		return Snapshot{}, err
	}
	var decoded file
	if _, err := toml.Decode(string(raw), &decoded); err != nil {
		return Snapshot{}, fmt.Errorf("parse %s: %w", path, err)
	}
	snapshot := Snapshot{Default: decoded.DefaultModel}
	for providerName, provider := range decoded.Providers {
		for modelID := range provider.Models {
			snapshot.Models = append(snapshot.Models, newModel(providerName, modelID))
		}
	}
	sort.Slice(snapshot.Models, func(i, j int) bool {
		return snapshot.Models[i].Selector < snapshot.Models[j].Selector
	})
	return snapshot, nil
}

func newModel(provider, modelID string) Model {
	return Model{Selector: provider + "/" + modelID, Provider: provider, ModelID: modelID}
}
