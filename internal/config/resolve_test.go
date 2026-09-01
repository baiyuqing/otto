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
	runtime, err := Resolve(file, map[string]string{"OTTO_MODEL": "env-model", "PROFILE_KEY": "secret"}, SessionDefaults{Provider: "openai-compatible", Model: "session-model"}, Overrides{Profile: "explicit"})
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
	runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{Provider: "codex", Model: "old-model"}, Overrides{Profile: "explicit"})
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

func TestResolveCopiesThinkingOverride(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", Model: "profile-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local", Thinking: "max"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Thinking != "max" {
		t.Fatalf("thinking = %q, want max", runtime.Thinking)
	}
}

func TestResolveUsesEnvironmentProfileBeforeDefaultProfile(t *testing.T) {
	file := File{
		DefaultProfile: "configured",
		Profiles: map[string]Profile{
			"configured": {Provider: "openai-compatible", Model: "config-model", BaseURL: "https://config.example/v1", APIKeyEnv: "CONFIG_KEY"},
			"current":    {Provider: "openai-compatible", Model: "current-model", BaseURL: "https://current.example/v1", APIKeyEnv: "CURRENT_KEY"},
		},
	}
	runtime, err := Resolve(file, map[string]string{"OTTO_PROFILE": "current", "CURRENT_KEY": "secret"}, SessionDefaults{}, Overrides{})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Profile != "current" || runtime.Model != "current-model" || runtime.APIKey != "secret" || runtime.BaseURL != "https://current.example/v1" {
		t.Fatalf("unexpected runtime: %#v", runtime)
	}
}

func TestResolveExplicitProfileOverridesEnvironmentProfile(t *testing.T) {
	file := File{
		DefaultProfile: "configured",
		Profiles: map[string]Profile{
			"env":      {Provider: "openai-compatible", Model: "env-model", BaseURL: "https://env.example/v1", APIKeyEnv: "ENV_KEY"},
			"explicit": {Provider: "openai-compatible", Model: "explicit-model", BaseURL: "https://explicit.example/v1", APIKeyEnv: "EXPLICIT_KEY"},
		},
	}
	runtime, err := Resolve(file, map[string]string{"OTTO_PROFILE": "env", "EXPLICIT_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "explicit"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Profile != "explicit" || runtime.Model != "explicit-model" || runtime.APIKey != "secret" || runtime.BaseURL != "https://explicit.example/v1" {
		t.Fatalf("unexpected runtime: %#v", runtime)
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

func TestResolveChatGPTProviderNeedsNoBaseURLOrKey(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"sub": {Provider: "chatgpt", Model: "gpt-5-codex"},
	}}
	runtime, err := Resolve(file, map[string]string{}, SessionDefaults{}, Overrides{Profile: "sub"})
	if err != nil {
		t.Fatalf("Resolve chatgpt: %v", err)
	}
	if runtime.Provider != "chatgpt" || runtime.Model != "gpt-5-codex" {
		t.Fatalf("unexpected runtime: %+v", runtime)
	}
	if runtime.BaseURL != "" || runtime.APIKey != "" {
		t.Fatalf("chatgpt runtime should carry no base_url/api key: %+v", runtime)
	}
}

func TestResolveChatGPTStillRequiresModel(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"sub": {Provider: "chatgpt"},
	}}
	if _, err := Resolve(file, nil, SessionDefaults{}, Overrides{Profile: "sub"}); err == nil || !strings.Contains(err.Error(), "model") {
		t.Fatalf("expected missing model error, got %v", err)
	}
}

func TestResolveRejectsMissingNamedAPIKeyEnvironmentVariable(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
	}}
	if _, err := Resolve(file, map[string]string{}, SessionDefaults{}, Overrides{Profile: "local"}); err == nil || !strings.Contains(err.Error(), "api key") {
		t.Fatalf("expected missing API key error, got %v", err)
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

func TestResolveRejectsInvalidShellTimeout(t *testing.T) {
	for _, timeout := range []string{"not-a-duration", "0s", "-1s"} {
		t.Run(timeout, func(t *testing.T) {
			file := File{
				Profiles: map[string]Profile{
					"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
				},
				Agent: Agent{ShellTimeout: timeout},
			}
			if _, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"}); err == nil || !strings.Contains(err.Error(), "shell_timeout") {
				t.Fatalf("expected invalid duration error, got %v", err)
			}
		})
	}
}

func TestResolveRejectsBaseURLsRejectedByOpenAIClient(t *testing.T) {
	for _, baseURL := range []string{
		"https://example.com/v1?tenant=x",
		"https://example.com/v1?",
		"https://example.com/v1#fragment",
		"https://username@example.com/v1",
		"https://username:password@example.com/v1",
		"ftp://example.com/v1",
		"http:///v1",
		"http://[::1",
	} {
		t.Run(baseURL, func(t *testing.T) {
			file := File{Profiles: map[string]Profile{
				"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: baseURL, APIKeyEnv: "PROFILE_KEY"},
			}}
			if _, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"}); err == nil || !strings.Contains(err.Error(), "base_url") {
				t.Fatalf("expected invalid base URL error, got %v", err)
			}
		})
	}
}

func TestResolveNormalizesBaseURLLikeOpenAIClient(t *testing.T) {
	file := File{Profiles: map[string]Profile{
		"local": {Provider: "openai-compatible", Model: "test-model", BaseURL: "https://example.com/gateway/v1/", APIKeyEnv: "PROFILE_KEY"},
	}}
	runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.BaseURL != "https://example.com/gateway/v1" {
		t.Fatalf("BaseURL = %q, want normalized URL", runtime.BaseURL)
	}
}

func TestResolvePrefersSessionDefaultsOverDefaultProfile(t *testing.T) {
	file := File{
		DefaultProfile: "configured",
		Profiles: map[string]Profile{
			"configured": {Provider: "openai-compatible", Model: "config-model", BaseURL: "https://config.example/v1", APIKeyEnv: "CONFIG_KEY"},
		},
	}
	runtime, err := Resolve(file, map[string]string{"CONFIG_KEY": "secret"}, SessionDefaults{Provider: "openai-compatible", Model: "session-model"}, Overrides{})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Model != "session-model" || runtime.BaseURL != "https://config.example/v1" {
		t.Fatalf("session did not win over default profile: %#v", runtime)
	}
}

func TestResolveCompactionAppliesDefaultsAndExplicitAuto(t *testing.T) {
	file := compactionTestFile("gpt-5.6-sol")
	runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	want := CompactionRuntime{
		Auto:             true,
		ContextWindow:    1_050_000,
		HardInputWindow:  922_000,
		WorkingWindow:    272_000,
		MaxOutputTokens:  128_000,
		ReserveTokens:    16_384,
		KeepRecentTokens: 20_000,
	}
	if runtime.Compaction != want {
		t.Fatalf("Compaction = %#v, want %#v", runtime.Compaction, want)
	}

	file.Agent.Compaction.Auto = boolPointer(false)
	runtime, err = Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Compaction.Auto {
		t.Fatal("Compaction.Auto = true, want explicit false")
	}
}

func TestResolveCompactionRejectsNonPositiveTargetsBeforeAPIKeyResolution(t *testing.T) {
	tests := []struct {
		name       string
		compaction CompactionConfig
		field      string
	}{
		{"zero reserve", CompactionConfig{ReserveTokens: intPointer(0)}, "reserve_tokens"},
		{"negative reserve", CompactionConfig{ReserveTokens: intPointer(-1)}, "reserve_tokens"},
		{"zero keep", CompactionConfig{KeepRecentTokens: intPointer(0)}, "keep_recent_tokens"},
		{"negative keep", CompactionConfig{KeepRecentTokens: intPointer(-1)}, "keep_recent_tokens"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			file := compactionTestFile("private-model")
			file.Agent.Compaction = test.compaction
			_, err := Resolve(file, nil, SessionDefaults{}, Overrides{Profile: "local"})
			if err == nil || !strings.Contains(err.Error(), test.field) {
				t.Fatalf("Resolve() error = %v, want %s validation error", err, test.field)
			}
			if strings.Contains(err.Error(), "api key") {
				t.Fatalf("Resolve() validated API key before compaction: %v", err)
			}
		})
	}
}

func TestResolveCompactionAppliesProfileOverridesAfterModelPrecedence(t *testing.T) {
	file := compactionTestFile("gpt-5.6-sol")
	profile := file.Profiles["local"]
	profile.ContextWindow = intPointer(131_072)
	profile.CompactionWindow = intPointer(100_000)
	file.Profiles["local"] = profile

	runtime, err := Resolve(
		file,
		map[string]string{"OTTO_MODEL": "private-deployment", "PROFILE_KEY": "secret"},
		SessionDefaults{Model: "session-model"},
		Overrides{Profile: "local", Model: "cli-private-deployment"},
	)
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Model != "cli-private-deployment" {
		t.Fatalf("Model = %q, want final CLI model", runtime.Model)
	}
	want := CompactionRuntime{
		Auto:             true,
		ContextWindow:    131_072,
		HardInputWindow:  131_072,
		WorkingWindow:    100_000,
		ReserveTokens:    16_384,
		KeepRecentTokens: 20_000,
	}
	if runtime.Compaction != want {
		t.Fatalf("Compaction = %#v, want %#v", runtime.Compaction, want)
	}
}

func TestResolveCompactionContextOverrideReplacesCatalogWindows(t *testing.T) {
	file := compactionTestFile("gpt-5.6-sol")
	profile := file.Profiles["local"]
	profile.ContextWindow = intPointer(65_536)
	file.Profiles["local"] = profile

	runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Compaction.ContextWindow != 65_536 || runtime.Compaction.HardInputWindow != 65_536 ||
		runtime.Compaction.WorkingWindow != 65_536 {
		t.Fatalf("profile context override did not replace all windows: %#v", runtime.Compaction)
	}
	if runtime.Compaction.MaxOutputTokens != 128_000 {
		t.Fatalf("MaxOutputTokens = %d, want catalog output limit", runtime.Compaction.MaxOutputTokens)
	}
}

func TestResolveCompactionRejectsInvalidProfileWindows(t *testing.T) {
	tests := []struct {
		name       string
		context    *int
		compaction *int
		field      string
	}{
		{"context zero", intPointer(0), nil, "context_window"},
		{"context negative", intPointer(-1), nil, "context_window"},
		{"context below minimum", intPointer(4_095), nil, "context_window"},
		{"compaction zero", intPointer(8_192), intPointer(0), "compaction_window"},
		{"compaction negative", intPointer(8_192), intPointer(-1), "compaction_window"},
		{"compaction below minimum", intPointer(8_192), intPointer(4_095), "compaction_window"},
		{"compaction without context", nil, intPointer(4_096), "context_window"},
		{"compaction exceeds context", intPointer(4_096), intPointer(4_097), "compaction_window"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			file := compactionTestFile("gpt-4o")
			profile := file.Profiles["local"]
			profile.ContextWindow = test.context
			profile.CompactionWindow = test.compaction
			file.Profiles["local"] = profile
			_, err := Resolve(file, nil, SessionDefaults{}, Overrides{Profile: "local"})
			if err == nil || !strings.Contains(err.Error(), test.field) {
				t.Fatalf("Resolve() error = %v, want %s validation error", err, test.field)
			}
			if strings.Contains(err.Error(), "api key") {
				t.Fatalf("Resolve() validated API key before profile limits: %v", err)
			}
		})
	}
}

func TestResolveCompactionAdjustsTargetsForSmallWindows(t *testing.T) {
	tests := []struct {
		name    string
		window  int
		reserve int
		keep    int
	}{
		{"minimum window", 4_096, 1_024, 1_536},
		{"eight thousand", 8_192, 2_048, 3_072},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			file := compactionTestFile("private-model")
			profile := file.Profiles["local"]
			profile.ContextWindow = intPointer(test.window)
			file.Profiles["local"] = profile
			runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"})
			if err != nil {
				t.Fatal(err)
			}
			if runtime.Compaction.ReserveTokens != test.reserve || runtime.Compaction.KeepRecentTokens != test.keep {
				t.Fatalf("Compaction targets = %d/%d, want %d/%d", runtime.Compaction.ReserveTokens,
					runtime.Compaction.KeepRecentTokens, test.reserve, test.keep)
			}
		})
	}
}

func TestResolveCompactionUnknownModelPreservesConfiguredTargets(t *testing.T) {
	file := compactionTestFile("private-model")
	file.Agent.Compaction.ReserveTokens = intPointer(99_999)
	file.Agent.Compaction.KeepRecentTokens = intPointer(88_888)
	runtime, err := Resolve(file, map[string]string{"PROFILE_KEY": "secret"}, SessionDefaults{}, Overrides{Profile: "local"})
	if err != nil {
		t.Fatal(err)
	}
	want := CompactionRuntime{Auto: true, ReserveTokens: 99_999, KeepRecentTokens: 88_888}
	if runtime.Compaction != want {
		t.Fatalf("Compaction = %#v, want %#v", runtime.Compaction, want)
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
	if runtime.ShellTimeout != 120*time.Second || runtime.MaxOutputBytes != 51200 {
		t.Fatalf("unexpected defaults: %#v", runtime)
	}
}

func compactionTestFile(model string) File {
	return File{Profiles: map[string]Profile{
		"local": {
			Provider:  "openai-compatible",
			Model:     model,
			BaseURL:   "https://example.com/v1",
			APIKeyEnv: "PROFILE_KEY",
		},
	}}
}

func boolPointer(value bool) *bool {
	return &value
}

func intPointer(value int) *int {
	return &value
}
