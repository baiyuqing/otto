package ottoconfig

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

const sample = `
default_model = "deepseek/deepseek-v4-flash"

[ui]
mode = ""

[providers.deepseek]
type = "openai-compatible"
base_url = "https://api.deepseek.example/v1"
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.models."deepseek-v4-flash"]
thinking = "high"

[providers.chatgpt.models."gpt-5.6-terra"]
[providers.chatgpt.models."gpt-5.5"]

[providers.openrouter.models."anthropic/claude-x"]

[server]
socket = ""
`

const legacySample = `
default_profile = "luna"

[profiles.luna]
provider = "chatgpt"
model = "gpt-5.6-luna"

[profiles.claude]
provider = "claude"
base_url = "https://claude.proxy.example"
model = "claude-x"

[profiles.sonnet]
provider = "claude"
base_url = "https://api.anthropic.com"
model = "claude-sonnet"

[profiles.deepseek]
provider = "openai-compatible"
base_url = "https://api.deepseek.example/v1"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-v4-flash"
`

func write(t *testing.T, text string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func selectors(snapshot Snapshot) []string {
	out := make([]string, 0, len(snapshot.Models))
	for _, model := range snapshot.Models {
		out = append(out, model.Selector)
	}
	return out
}

func TestReadParsesSortedSelectorsAndTheDefault(t *testing.T) {
	t.Parallel()
	snapshot, err := Read(write(t, sample))
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Default != "deepseek/deepseek-v4-flash" {
		t.Errorf("default = %q", snapshot.Default)
	}
	want := []string{
		"chatgpt/gpt-5.5",
		"chatgpt/gpt-5.6-terra",
		"deepseek/deepseek-v4-flash",
		"openrouter/anthropic/claude-x",
	}
	if got := selectors(snapshot); !reflect.DeepEqual(got, want) {
		t.Errorf("selectors = %v, want %v", got, want)
	}
	last := snapshot.Models[3]
	if last.Provider != "openrouter" || last.ModelID != "anthropic/claude-x" {
		t.Errorf("selector split = %+v, want provider openrouter and model ID anthropic/claude-x", last)
	}
}

func TestReadConvertsLegacyProfilesToSelectors(t *testing.T) {
	t.Parallel()
	snapshot, err := Read(write(t, legacySample))
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Default != "chatgpt/gpt-5.6-luna" {
		t.Errorf("default = %q, want the converted default_profile", snapshot.Default)
	}
	want := []string{
		"chatgpt/gpt-5.6-luna",       // default connection: provider type
		"claude-custom/claude-x",     // profile named claude with a custom base_url
		"claude/claude-sonnet",       // explicit Anthropic defaults are still default
		"deepseek/deepseek-v4-flash", // custom connection: profile name
	}
	if got := selectors(snapshot); !reflect.DeepEqual(got, want) {
		t.Errorf("selectors = %v, want %v", got, want)
	}
}

func TestReadRejectsAFileMixingLegacyAndModelKeys(t *testing.T) {
	t.Parallel()
	for name, text := range map[string]string{
		"table":   "[profiles.a]\nprovider = \"chatgpt\"\nmodel = \"m\"\n\n[providers.chatgpt.models.\"m\"]\n",
		"default": "default_profile = \"a\"\ndefault_model = \"chatgpt/m\"\n",
	} {
		if _, err := Read(write(t, text)); err == nil {
			t.Errorf("%s: a file with legacy and new keys must be an error", name)
		}
	}
}

func TestReadMissingFileIsAnEmptySnapshot(t *testing.T) {
	t.Parallel()
	snapshot, err := Read(filepath.Join(t.TempDir(), "config.toml"))
	if err != nil {
		t.Fatalf("a missing file must not be an error: %v", err)
	}
	if snapshot.Default != "" || len(snapshot.Models) != 0 {
		t.Errorf("snapshot = %+v, want empty", snapshot)
	}
}

func TestReadRejectsBrokenTOML(t *testing.T) {
	t.Parallel()
	if _, err := Read(write(t, "[providers.deepseek\nmodel = ")); err == nil {
		t.Fatal("a malformed file must be an error")
	}
}

func TestPathIsTheUserConfiguration(t *testing.T) {
	t.Parallel()
	if got, want := Path("/home/me"), "/home/me/.config/otto/config.toml"; got != want {
		t.Errorf("Path = %q, want %q", got, want)
	}
}
