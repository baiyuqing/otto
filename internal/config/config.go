package config

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"

	toml "github.com/pelletier/go-toml/v2"
)

// Provider identifiers accepted in config, env, and flags.
const (
	// ProviderOpenAICompatible uses a base_url + API key (Stage 1).
	ProviderOpenAICompatible = "openai-compatible"
	// ProviderChatGPT uses a ChatGPT subscription via OAuth credentials.
	ProviderChatGPT = "chatgpt"
)

type File struct {
	DefaultProfile string             `toml:"default_profile"`
	UI             UI                 `toml:"ui"`
	Agent          Agent              `toml:"agent"`
	Memory         Memory             `toml:"memory"`
	Skills         Skills             `toml:"skills"`
	Server         Server             `toml:"server"`
	Sandbox        SandboxConfig      `toml:"sandbox"`
	Profiles       map[string]Profile `toml:"profiles"`
}

type SandboxConfig struct {
	Driver    *string  `toml:"driver"`
	Network   *string  `toml:"network"`
	ReadPaths []string `toml:"read_paths"`
	AllowEnv  []string `toml:"allow_env"`
}

type Agent struct {
	MaxTurns       int              `toml:"max_turns"`
	ShellTimeout   string           `toml:"shell_timeout"`
	MaxOutputBytes int              `toml:"max_output_bytes"`
	Compaction     CompactionConfig `toml:"compaction"`
}

type CompactionConfig struct {
	Auto             *bool `toml:"auto"`
	ReserveTokens    *int  `toml:"reserve_tokens"`
	KeepRecentTokens *int  `toml:"keep_recent_tokens"`
}

type Profile struct {
	Provider         string `toml:"provider"`
	BaseURL          string `toml:"base_url"`
	Model            string `toml:"model"`
	APIKeyEnv        string `toml:"api_key_env"`
	ContextWindow    *int   `toml:"context_window"`
	CompactionWindow *int   `toml:"compaction_window"`
}

type SessionDefaults struct {
	Provider string
	Model    string
}

type Overrides struct {
	Profile        string
	Provider       string
	BaseURL        string
	Model          string
	Thinking       string
	ShellTimeout   time.Duration
	MaxOutputBytes int
}

type CompactionRuntime struct {
	Auto             bool
	ContextWindow    int
	HardInputWindow  int
	WorkingWindow    int
	MaxOutputTokens  int
	ReserveTokens    int
	KeepRecentTokens int
}

type Runtime struct {
	Profile        string
	Provider       string
	BaseURL        string
	Model          string
	Thinking       string
	APIKey         string
	APIKeyEnv      string
	ShellTimeout   time.Duration
	MaxOutputBytes int
	Compaction     CompactionRuntime
}

func Save(path string, file File) error {
	data, err := toml.Marshal(file)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	return os.WriteFile(path, data, 0o600)
}

func SetDefaultProfile(path, profile string) error {
	if profile == "" {
		return fmt.Errorf("missing profile")
	}
	file, err := Load(path)
	if err != nil {
		return err
	}
	if _, ok := file.Profiles[profile]; !ok {
		return fmt.Errorf("profile %q not found", profile)
	}
	content, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) && path == DefaultPath() {
			file.DefaultProfile = profile
			return Save(path, file)
		}
		return err
	}
	updated := replaceDefaultProfile(string(content), profile)
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	return os.WriteFile(path, []byte(updated), 0o600)
}

var defaultProfileLineRE = regexp.MustCompile(`(?m)^\s*default_profile\s*=\s*("(?:[^"\\]|\\.)*"|'[^']*')\s*(#.*)?$`)

func replaceDefaultProfile(content, profile string) string {
	line := "default_profile = " + strconv.Quote(profile)
	if defaultProfileLineRE.MatchString(content) {
		return defaultProfileLineRE.ReplaceAllString(content, line)
	}
	if strings.TrimSpace(content) == "" {
		return line + "\n"
	}
	if strings.HasSuffix(content, "\n") {
		return line + "\n" + content
	}
	return line + "\n" + content + "\n"
}

func DefaultPath() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join("~", ".config", "otto", "config.toml")
	}
	return filepath.Join(home, ".config", "otto", "config.toml")
}

func Load(path string) (File, error) {
	cfg, err := LoadRequired(path)
	if err != nil && os.IsNotExist(err) && path == DefaultPath() {
		return File{}, nil
	}
	return cfg, err
}

func LoadRequired(path string) (File, error) {
	file, err := os.Open(path)
	if err != nil {
		return File{}, err
	}
	defer file.Close()

	var cfg File
	decoder := toml.NewDecoder(file)
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&cfg); err != nil {
		if strings.Contains(err.Error(), "strict mode") || strings.Contains(err.Error(), "missing in the target struct") || strings.Contains(err.Error(), "unknown") {
			return File{}, fmt.Errorf("unknown field: %w", err)
		}
		return File{}, err
	}
	if cfg.Profiles == nil {
		cfg.Profiles = map[string]Profile{}
	}
	return cfg, nil
}
