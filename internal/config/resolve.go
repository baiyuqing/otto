package config

import (
	"fmt"
	"time"

	"github.com/baiyuqing/otto/internal/provider/openaicompat"
)

const (
	defaultMaxTurns       = 20
	defaultShellTimeout   = 120 * time.Second
	defaultMaxOutputBytes = 51200
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
		provider  string
		model     string
		baseURL   string
		apiKeyEnv string
	)

	if selectedProfile != "" {
		profile, ok := file.Profiles[selectedProfile]
		if !ok {
			return Runtime{}, fmt.Errorf("profile %q not found", selectedProfile)
		}
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
	if provider != "openai-compatible" {
		return Runtime{}, fmt.Errorf("unsupported provider %q", provider)
	}
	if model == "" {
		return Runtime{}, fmt.Errorf("missing model")
	}
	if baseURL == "" {
		return Runtime{}, fmt.Errorf("missing base_url")
	}
	normalizedBaseURL, err := openaicompat.NormalizeBaseURL(baseURL)
	if err != nil {
		return Runtime{}, fmt.Errorf("invalid base_url: %w", err)
	}
	baseURL = normalizedBaseURL

	maxTurns := defaultMaxTurns
	if file.Agent.MaxTurns > 0 {
		maxTurns = file.Agent.MaxTurns
	}
	if overrides.MaxTurns > 0 {
		maxTurns = overrides.MaxTurns
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

	apiKey, err := resolveAPIKey(env, apiKeyEnv)
	if err != nil {
		return Runtime{}, err
	}

	return Runtime{
		Profile:        runtime.Profile,
		Provider:       provider,
		BaseURL:        baseURL,
		Model:          model,
		APIKey:         apiKey,
		APIKeyEnv:      apiKeyEnv,
		MaxTurns:       maxTurns,
		ShellTimeout:   shellTimeout,
		MaxOutputBytes: maxOutputBytes,
	}, nil
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
