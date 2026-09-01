package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestSetDefaultProfileUpdatesExistingLine(t *testing.T) {
	path := writeConfig(t, `default_profile = "old"
[profiles.old]
provider = "openai-compatible"
model = "old-model"
base_url = "https://old.example/v1"
api_key_env = "OLD_KEY"
[profiles.new]
provider = "chatgpt"
model = "gpt-5-codex"
`)
	if err := SetDefaultProfile(path, "new"); err != nil {
		t.Fatal(err)
	}
	content := string(mustReadFile(t, path))
	if !strings.Contains(content, `default_profile = "new"`) || strings.Contains(content, `default_profile = "old"`) {
		t.Fatalf("content = %q", content)
	}
	file, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.DefaultProfile != "new" || file.Profiles["old"].Model != "old-model" || file.Profiles["new"].Provider != "chatgpt" {
		t.Fatalf("file = %#v", file)
	}
}

func TestSetDefaultProfileInsertsMissingLine(t *testing.T) {
	path := writeConfig(t, `[profiles.new]
provider = "chatgpt"
model = "gpt-5-codex"
`)
	if err := SetDefaultProfile(path, "new"); err != nil {
		t.Fatal(err)
	}
	content := string(mustReadFile(t, path))
	if !strings.HasPrefix(content, "default_profile = \"new\"\n") {
		t.Fatalf("content = %q", content)
	}
}

func TestSetDefaultProfileRejectsUnknownProfile(t *testing.T) {
	path := writeConfig(t, `[profiles.known]
provider = "chatgpt"
model = "gpt-5-codex"
`)
	if err := SetDefaultProfile(path, "missing"); err == nil || !strings.Contains(err.Error(), `profile "missing" not found`) {
		t.Fatalf("error = %v, want missing profile", err)
	}
}

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

func mustReadFile(t *testing.T, path string) []byte {
	t.Helper()
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return content
}

func writeConfig(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}
