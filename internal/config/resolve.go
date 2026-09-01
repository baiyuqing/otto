package config

import (
	"fmt"
	"time"

	"github.com/baiyuqing/otto/internal/provider/openaicompat"
)

const (
	defaultShellTimeout      = 120 * time.Second
	defaultMaxOutputBytes    = 51200
	defaultCompactionReserve = 16_384
	defaultCompactionKeep    = 20_000
	minimumCompactionWindow  = 4_096
	minimumCompactionTarget  = 1_024
)

func Resolve(file File, env map[string]string, session SessionDefaults, overrides Overrides) (Runtime, error) {
	runtime := Runtime{
		Profile: overrides.Profile,
	}

	explicitProfile := overrides.Profile != ""
	selectedProfile := overrides.Profile
	if selectedProfile == "" {
		selectedProfile = file.DefaultProfile
		if selectedProfile != "" {
			runtime.Profile = selectedProfile
		}
	}

	var (
		provider      string
		model         string
		baseURL       string
		apiKeyEnv     string
		profileConfig Profile
	)

	if selectedProfile != "" {
		profile, ok := file.Profiles[selectedProfile]
		if !ok {
			return Runtime{}, fmt.Errorf("profile %q not found", selectedProfile)
		}
		profileConfig = profile
		provider = profile.Provider
		model = profile.Model
		baseURL = profile.BaseURL
		apiKeyEnv = profile.APIKeyEnv
	}
	if !explicitProfile {
		if session.Provider != "" {
			provider = session.Provider
		}
		if session.Model != "" {
			model = session.Model
		}
	}

	if v := envValue(env, "OTTO_PROVIDER"); v != "" {
		provider = v
	}
	if v := envValue(env, "OTTO_MODEL"); v != "" {
		model = v
	}

	if overrides.Provider != "" {
		provider = overrides.Provider
	}
	if overrides.Model != "" {
		model = overrides.Model
	}
	if overrides.BaseURL != "" {
		baseURL = overrides.BaseURL
	}

	if provider == "" {
		return Runtime{}, fmt.Errorf("missing provider")
	}
	switch provider {
	case ProviderOpenAICompatible, ProviderChatGPT:
	default:
		return Runtime{}, fmt.Errorf("unsupported provider %q", provider)
	}
	if model == "" {
		return Runtime{}, fmt.Errorf("missing model")
	}
	if provider == ProviderOpenAICompatible {
		if baseURL == "" {
			return Runtime{}, fmt.Errorf("missing base_url")
		}
		normalizedBaseURL, err := openaicompat.NormalizeBaseURL(baseURL)
		if err != nil {
			return Runtime{}, fmt.Errorf("invalid base_url: %w", err)
		}
		baseURL = normalizedBaseURL
	} else {
		// The chatgpt provider uses a fixed backend URL and OAuth credentials
		// from the credential file, not a configured base_url or API key.
		baseURL = ""
	}

	shellTimeout := defaultShellTimeout
	if file.Agent.ShellTimeout != "" {
		duration, err := time.ParseDuration(file.Agent.ShellTimeout)
		if err != nil {
			return Runtime{}, fmt.Errorf("invalid shell_timeout: %w", err)
		}
		if duration <= 0 {
			return Runtime{}, fmt.Errorf("invalid shell_timeout: must be greater than zero")
		}
		shellTimeout = duration
	}
	if overrides.ShellTimeout > 0 {
		shellTimeout = overrides.ShellTimeout
	}

	maxOutputBytes := defaultMaxOutputBytes
	if file.Agent.MaxOutputBytes > 0 {
		maxOutputBytes = file.Agent.MaxOutputBytes
	}
	if overrides.MaxOutputBytes > 0 {
		maxOutputBytes = overrides.MaxOutputBytes
	}

	compaction, err := resolveCompaction(file.Agent.Compaction, profileConfig, model)
	if err != nil {
		return Runtime{}, err
	}

	var apiKey string
	if provider == ProviderOpenAICompatible {
		apiKey, err = resolveAPIKey(env, apiKeyEnv)
		if err != nil {
			return Runtime{}, err
		}
	}

	return Runtime{
		Profile:        runtime.Profile,
		Provider:       provider,
		BaseURL:        baseURL,
		Model:          model,
		Thinking:       overrides.Thinking,
		APIKey:         apiKey,
		APIKeyEnv:      apiKeyEnv,
		ShellTimeout:   shellTimeout,
		MaxOutputBytes: maxOutputBytes,
		Compaction:     compaction,
	}, nil
}

func resolveCompaction(config CompactionConfig, profile Profile, model string) (CompactionRuntime, error) {
	auto := true
	if config.Auto != nil {
		auto = *config.Auto
	}

	reserve := defaultCompactionReserve
	if config.ReserveTokens != nil {
		if *config.ReserveTokens <= 0 {
			return CompactionRuntime{}, fmt.Errorf("invalid reserve_tokens: must be greater than zero")
		}
		reserve = *config.ReserveTokens
	}

	keep := defaultCompactionKeep
	if config.KeepRecentTokens != nil {
		if *config.KeepRecentTokens <= 0 {
			return CompactionRuntime{}, fmt.Errorf("invalid keep_recent_tokens: must be greater than zero")
		}
		keep = *config.KeepRecentTokens
	}

	if profile.ContextWindow != nil && *profile.ContextWindow < minimumCompactionWindow {
		return CompactionRuntime{}, fmt.Errorf("invalid context_window: must be at least %d", minimumCompactionWindow)
	}
	if profile.CompactionWindow != nil {
		if profile.ContextWindow == nil {
			return CompactionRuntime{}, fmt.Errorf("invalid context_window: required with compaction_window")
		}
		if *profile.CompactionWindow < minimumCompactionWindow {
			return CompactionRuntime{}, fmt.Errorf("invalid compaction_window: must be at least %d", minimumCompactionWindow)
		}
		if *profile.CompactionWindow > *profile.ContextWindow {
			return CompactionRuntime{}, fmt.Errorf("invalid compaction_window: must not exceed context_window")
		}
	}

	limits := resolveModelLimits(model)
	runtime := CompactionRuntime{
		Auto:             auto,
		ContextWindow:    limits.ContextWindow,
		HardInputWindow:  limits.HardInputWindow,
		WorkingWindow:    limits.WorkingWindow,
		MaxOutputTokens:  limits.MaxOutputTokens,
		ReserveTokens:    reserve,
		KeepRecentTokens: keep,
	}
	if profile.ContextWindow != nil {
		runtime.ContextWindow = *profile.ContextWindow
		runtime.HardInputWindow = *profile.ContextWindow
		runtime.WorkingWindow = *profile.ContextWindow
	}
	if profile.CompactionWindow != nil {
		runtime.WorkingWindow = *profile.CompactionWindow
	}

	if runtime.WorkingWindow > 0 {
		reserveCap := max(minimumCompactionTarget, runtime.WorkingWindow/4)
		runtime.ReserveTokens = min(runtime.ReserveTokens, reserveCap)
		keepCap := max(minimumCompactionTarget, (runtime.WorkingWindow-runtime.ReserveTokens)/2)
		runtime.KeepRecentTokens = min(runtime.KeepRecentTokens, keepCap)
	}
	return runtime, nil
}

func envValue(env map[string]string, key string) string {
	if env == nil {
		return ""
	}
	return env[key]
}

func resolveAPIKey(env map[string]string, apiKeyEnv string) (string, error) {
	if apiKeyEnv != "" {
		if value := envValue(env, apiKeyEnv); value != "" {
			return value, nil
		}
		if fallback := envValue(env, "OTTO_API_KEY"); fallback != "" {
			return fallback, nil
		}
		return "", fmt.Errorf("missing api key")
	}
	if fallback := envValue(env, "OTTO_API_KEY"); fallback != "" {
		return fallback, nil
	}
	return "", fmt.Errorf("missing api key")
}
