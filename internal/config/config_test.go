package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestLoadRejectsUnknownFields(t *testing.T) {
	path := writeConfig(t, `default_profile = "local"
unknown = true
[profiles.local]
provider = "openai-compatible"
model = "test-model"
base_url = "http://localhost:8080/v1"
api_key_env = "TEST_KEY"
`)
	_, err := Load(path)
	if err == nil || !strings.Contains(err.Error(), "unknown") {
		t.Fatalf("expected unknown-field error, got %v", err)
	}
}

func TestLoadCompactionPreservesAbsentAndExplicitValues(t *testing.T) {
	path := writeConfig(t, `[agent.compaction]
auto = false
reserve_tokens = 0
keep_recent_tokens = -1

[profiles.local]
context_window = 0
compaction_window = -2
`)
	file, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.Agent.Compaction.Auto == nil || *file.Agent.Compaction.Auto {
		t.Fatalf("compaction auto = %#v, want explicit false", file.Agent.Compaction.Auto)
	}
	if file.Agent.Compaction.ReserveTokens == nil || *file.Agent.Compaction.ReserveTokens != 0 {
		t.Fatalf("reserve_tokens = %#v, want explicit zero", file.Agent.Compaction.ReserveTokens)
	}
	if file.Agent.Compaction.KeepRecentTokens == nil || *file.Agent.Compaction.KeepRecentTokens != -1 {
		t.Fatalf("keep_recent_tokens = %#v, want explicit negative", file.Agent.Compaction.KeepRecentTokens)
	}
	profile := file.Profiles["local"]
	if profile.ContextWindow == nil || *profile.ContextWindow != 0 {
		t.Fatalf("context_window = %#v, want explicit zero", profile.ContextWindow)
	}
	if profile.CompactionWindow == nil || *profile.CompactionWindow != -2 {
		t.Fatalf("compaction_window = %#v, want explicit negative", profile.CompactionWindow)
	}

	path = writeConfig(t, "")
	file, err = Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.Agent.Compaction.Auto != nil || file.Agent.Compaction.ReserveTokens != nil ||
		file.Agent.Compaction.KeepRecentTokens != nil {
		t.Fatalf("absent compaction values = %#v, want nil pointers", file.Agent.Compaction)
	}
}

func TestLoadCompactionRejectsUnknownFields(t *testing.T) {
	path := writeConfig(t, `[agent.compaction]
unknown = true
`)
	if _, err := Load(path); err == nil || !strings.Contains(err.Error(), "unknown") {
		t.Fatalf("expected unknown-field error, got %v", err)
	}
}

func TestLoadUIDecodesMode(t *testing.T) {
	path := writeConfig(t, `[ui]
mode = "auto"
`)
	file, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := file.UI.Mode; got != "auto" {
		t.Fatalf("UI.Mode = %q, want auto", got)
	}
}

func TestLoadUIRejectsUnknownFields(t *testing.T) {
	path := writeConfig(t, `[ui]
mode = "auto"
unknown = true
`)
	if _, err := Load(path); err == nil || !strings.Contains(err.Error(), "unknown") {
		t.Fatalf("expected unknown-field error, got %v", err)
	}
}

func TestLoadSandboxPreservesAbsentAndExplicitValues(t *testing.T) {
	path := writeConfig(t, "")
	file, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.Sandbox.Driver != nil || file.Sandbox.Network != nil || file.Sandbox.ReadPaths != nil || file.Sandbox.AllowEnv != nil {
		t.Fatalf("absent sandbox config = %#v, want zero value", file.Sandbox)
	}

	path = writeConfig(t, `[sandbox]
driver = ""
network = ""
read_paths = ["/opt/sdk", "~/source"]
allow_env = ["PATH", "PROJECT_TOKEN"]
`)
	file, err = Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.Sandbox.Driver == nil || *file.Sandbox.Driver != "" {
		t.Fatalf("sandbox driver = %#v, want explicit empty string", file.Sandbox.Driver)
	}
	if file.Sandbox.Network == nil || *file.Sandbox.Network != "" {
		t.Fatalf("sandbox network = %#v, want explicit empty string", file.Sandbox.Network)
	}
	if got := strings.Join(file.Sandbox.ReadPaths, ","); got != "/opt/sdk,~/source" {
		t.Fatalf("sandbox read_paths = %q, want decoded values", got)
	}
	if got := strings.Join(file.Sandbox.AllowEnv, ","); got != "PATH,PROJECT_TOKEN" {
		t.Fatalf("sandbox allow_env = %q, want decoded values", got)
	}
}

func TestLoadSandboxDecodesValidTable(t *testing.T) {
	path := writeConfig(t, `[sandbox]
driver = "seatbelt"
network = "deny"
read_paths = ["/Library/Developer"]
allow_env = ["PROJECT_TOKEN"]
`)
	file, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.Sandbox.Driver == nil || *file.Sandbox.Driver != "seatbelt" ||
		file.Sandbox.Network == nil || *file.Sandbox.Network != "deny" {
		t.Fatalf("sandbox modes were not decoded: %#v", file.Sandbox)
	}
}

func TestLoadSandboxRejectsUnknownFields(t *testing.T) {
	path := writeConfig(t, `[sandbox]
driver = "auto"
unknown = true
`)
	if _, err := Load(path); err == nil || !strings.Contains(err.Error(), "unknown") {
		t.Fatalf("expected unknown sandbox field error, got %v", err)
	}
}

func TestLoadRejectsRawSecretField(t *testing.T) {
	path := writeConfig(t, `[profiles.bad]
provider = "openai-compatible"
model = "test-model"
base_url = "https://example.com/v1"
api_key = "secret"
`)
	if _, err := Load(path); err == nil {
		t.Fatal("expected raw api_key to be rejected")
	}
}

func TestLoadReturnsEmptyFileForMissingDefaultPath(t *testing.T) {
	t.Setenv("HOME", t.TempDir())

	file, err := Load(DefaultPath())
	if err != nil {
		t.Fatal(err)
	}
	if file.DefaultProfile != "" || len(file.Profiles) != 0 {
		t.Fatalf("expected empty file, got %#v", file)
	}
}

func TestLoadRejectsMissingExplicitPath(t *testing.T) {
	path := filepath.Join(t.TempDir(), "missing.toml")
	if _, err := Load(path); err == nil {
		t.Fatal("expected missing explicit path to fail")
	}
}

func TestDefaultPathUsesHomeDir(t *testing.T) {
	t.Setenv("HOME", filepath.Join(t.TempDir(), "home"))
	want := filepath.Join(os.Getenv("HOME"), ".config", "otto", "config.toml")
	if got := DefaultPath(); got != want {
		t.Fatalf("DefaultPath() = %q, want %q", got, want)
	}
}

func writeConfig(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}
