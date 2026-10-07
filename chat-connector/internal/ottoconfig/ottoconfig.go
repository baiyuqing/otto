// Package ottoconfig reads the part of Otto's local user configuration that
// the connector displays in its profile picker.
//
// It reads one file, ~/.config/otto/config.toml, and returns only profile
// names, providers and models. It never returns credentials: Otto's
// configuration stores the names of API-key environment variables, never their
// values, so nothing secret can be read here. Otto itself remains the writer of
// this file.
package ottoconfig

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"github.com/BurntSushi/toml"
)

// Profile is one configured provider profile.
type Profile struct {
	Name     string
	Provider string
	Model    string
}

// Snapshot is the local user configuration the picker needs.
type Snapshot struct {
	// Default is the configured default_profile; empty when unset.
	Default string
	// Profiles is sorted by name.
	Profiles []Profile
}

// Path is the user configuration source Otto reads by default.
func Path(home string) string {
	return filepath.Join(home, ".config", "otto", "config.toml")
}

// file declares only the decoded subset. Unknown keys and sections are
// ignored, so a newer Otto configuration still loads.
type file struct {
	DefaultProfile string `toml:"default_profile"`
	Profiles       map[string]struct {
		Provider string `toml:"provider"`
		Model    string `toml:"model"`
	} `toml:"profiles"`
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
	snapshot := Snapshot{Default: decoded.DefaultProfile}
	for name, profile := range decoded.Profiles {
		snapshot.Profiles = append(snapshot.Profiles, Profile{
			Name:     name,
			Provider: profile.Provider,
			Model:    profile.Model,
		})
	}
	sort.Slice(snapshot.Profiles, func(i, j int) bool {
		return snapshot.Profiles[i].Name < snapshot.Profiles[j].Name
	})
	return snapshot, nil
}
