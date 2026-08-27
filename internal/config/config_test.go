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

func writeConfig(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}
