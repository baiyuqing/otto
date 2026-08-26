package config

import (
	"strings"
	"testing"
	"time"
)

func TestResolvePrecedence(t *testing.T) {
	file := File{
		DefaultProfile: "configured",
		Profiles: map[string]Profile{
			"configured": {Provider: "openai-compatible", Model: "config-model", BaseURL: "https://config.example/v1", APIKeyEnv: "CONFIG_KEY"},
			"explicit":   {Provider: "openai-compatible", Model: "profile-model", BaseURL: "https://profile.example/v1", APIKeyEnv: "PROFILE_KEY"},
		},
	}
	runtime, err := Resolve(file, map[string]string{"OTTO_MODEL": "env-model"}, SessionDefaults{Provider: "openai-compatible", Model: "session-model"}, Overrides{Profile: "explicit"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Model != "env-model" || runtime.Profile != "explicit" || runtime.BaseURL != "https://profile.example/v1" {
		t.Fatalf("unexpected runtime: %#v", runtime)
	}
}

func TestExplicitProfileOverridesResumedProviderAndModel(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"explicit": {Provider: "openai-compatible", Model: "profile-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	runtime, err := Resolve(file, nil, SessionDefaults{Provider: "codex", Model: "old-model"}, Overrides{Profile: "explicit"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Provider != "openai-compatible" || runtime.Model != "profile-model" {
		t.Fatalf("explicit profile did not win: %#v", runtime)
	}
}

func TestResolvePrefersCLIOverridesOverEnvironment(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", Model: "profile-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	runtime, err := Resolve(file, map[string]string{"OTTO_PROVIDER": "openai-compatible", "OTTO_MODEL": "env-model", "PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local", Model: "cli-model"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Model != "cli-model" {
		t.Fatalf("CLI model did not win: %#v", runtime)
	}
}

func TestResolveRejectsUnknownProfile(t *testing.T) {
	if _, err := Resolve(File{}, nil, SessionDefaults{}, Overrides{Profile: "missing"}); err == nil || !strings.Contains(err.Error(), "missing") {
		t.Fatalf("expected missing profile error, got %v", err)
	}
}

func TestResolveRejectsMissingModel(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	if _, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"}); err == nil || !strings.Contains(err.Error(), "model") {
		t.Fatalf("expected missing model error, got %v", err)
	}
}

func TestResolveRejectsUnsupportedProvider(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "codex", Model: "test-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	if _, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"}); err == nil || !strings.Contains(err.Error(), "provider") {
		t.Fatalf("expected unsupported provider error, got %v", err)
	}
}

func TestResolveHandlesMissingNamedAPIKeyEnvironmentVariable(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	runtime, err := Resolve(file, map[string]string{}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.APIKey != "" {
		t.Fatalf("APIKey = %q, want empty", runtime.APIKey)
	}
}

func TestResolveUsesOTTOAPIKeyFallback(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: "https://example.com/v1"},
	}}
	runtime, err := Resolve(file, map[string]string{"OTTO_API_KEY": "fallback-secret"}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.APIKey != "fallback-secret" {
		t.Fatalf("APIKey = %q, want fallback-secret", runtime.APIKey)
	}
}

func TestResolveParsesInvalidShellTimeout(t *testing.T) {
	file := File{
		Profiles: map[string]Profile{
			"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
		},
		Agent: Agent{ShellTimeout: "not-a-duration"},
	}
	if _, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"}); err == nil || !strings.Contains(err.Error(), "shell_timeout") {
		t.Fatalf("expected invalid duration error, got %v", err)
	}
}

func TestResolveAppliesAgentDefaults(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.MaxTurns != 20 || runtime.ShellTimeout != 120*time.Second || runtime.MaxOutputBytes != 51200 {
		t.Fatalf("unexpected defaults: %#v", runtime)
	}
}
