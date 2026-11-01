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
	// Default is the selector of default_model (or of the converted
	// default_profile); empty when unset.
	Default string
	// Models is sorted by selector, without duplicates.
	Models []Model
}

// Path is the user configuration source Otto reads by default.
func Path(home string) string {
	return filepath.Join(home, ".config", "otto", "config.toml")
}

// builtinProviders are the provider names Otto reserves for the three API types.
var builtinProviders = map[string]bool{"claude": true, "openai-compatible": true, "chatgpt": true}

// claudeDefaultBaseURL and claudeDefaultKeyEnv are the Claude connection
// defaults; a legacy profile that sets them explicitly is still a default one.
const (
	claudeDefaultBaseURL = "https://api.anthropic.com"
	claudeDefaultKeyEnv  = "ANTHROPIC_API_KEY"
)

// legacyProfile is the decoded subset of a [profiles.<name>] table.
type legacyProfile struct {
	Provider       string `toml:"provider"`
	Model          string `toml:"model"`
	BaseURL        string `toml:"base_url"`
	APIKeyEnv      string `toml:"api_key_env"`
	APIVersion     string `toml:"api_version"`
	WorkspaceIDEnv string `toml:"workspace_id_env"`
}

// providerTable is the decoded subset of a [providers.<name>] table. Model
// tables are decoded as free maps: only their keys are needed.
type providerTable struct {
	Models map[string]map[string]any `toml:"models"`
}

// file declares only the decoded subset. Unknown keys and sections are
// ignored, so a newer Otto configuration still loads.
type file struct {
	DefaultModel   string                   `toml:"default_model"`
	Providers      map[string]providerTable `toml:"providers"`
	DefaultProfile string                   `toml:"default_profile"`
	Profiles       map[string]legacyProfile `toml:"profiles"`
}

// Read parses path. A missing file is an empty snapshot rather than an error:
// Otto may be configured entirely through the workspace or managed layers.
// A file that holds both the legacy profile keys and the model keys is an
// error, as it is in Otto.
func Read(path string) (Snapshot, error) {
	raw, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return Snapshot{}, nil
	}
	if err != nil {
		return Snapshot{}, err
	}
	var decoded file
	meta, err := toml.Decode(string(raw), &decoded)
	if err != nil {
		return Snapshot{}, fmt.Errorf("parse %s: %w", path, err)
	}
	legacy := meta.IsDefined("profiles") || meta.IsDefined("default_profile")
	current := meta.IsDefined("providers") || meta.IsDefined("default_model")
	if legacy && current {
		return Snapshot{}, fmt.Errorf("%s mixes profiles/default_profile with providers/default_model", path)
	}
	var snapshot Snapshot
	if legacy {
		snapshot = convertLegacy(decoded)
	} else {
		snapshot = Snapshot{Default: decoded.DefaultModel}
		for providerName, provider := range decoded.Providers {
			for modelID := range provider.Models {
				snapshot.Models = append(snapshot.Models, newModel(providerName, modelID))
			}
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

// convertLegacy maps each legacy profile to the selector Otto converts it to,
// in simplified form: profiles that share a custom connection are not merged
// into one provider, and two profiles with the same selector appear once.
func convertLegacy(decoded file) Snapshot {
	var snapshot Snapshot
	selectors := make(map[string]string, len(decoded.Profiles)) // profile name -> selector
	seen := make(map[string]bool, len(decoded.Profiles))
	for name, profile := range decoded.Profiles {
		model := newModel(legacyProviderName(name, profile), profile.Model)
		selectors[name] = model.Selector
		if !seen[model.Selector] {
			seen[model.Selector] = true
			snapshot.Models = append(snapshot.Models, model)
		}
	}
	snapshot.Default = selectors[decoded.DefaultProfile]
	return snapshot
}

// legacyProviderName names the provider a legacy profile converts to: the
// built-in provider of its type when the connection fields are all default,
// otherwise the profile name, with "-custom" appended when that name is
// reserved for a built-in provider.
func legacyProviderName(name string, profile legacyProfile) string {
	if legacyConnectionIsDefault(profile) {
		return profile.Provider
	}
	if builtinProviders[name] {
		return name + "-custom"
	}
	return name
}

func legacyConnectionIsDefault(profile legacyProfile) bool {
	if profile.APIVersion != "" || profile.WorkspaceIDEnv != "" {
		return false
	}
	baseURL, keyEnv := profile.BaseURL, profile.APIKeyEnv
	if profile.Provider == "claude" {
		if baseURL == claudeDefaultBaseURL {
			baseURL = ""
		}
		if keyEnv == claudeDefaultKeyEnv {
			keyEnv = ""
		}
	}
	return baseURL == "" && keyEnv == ""
}
