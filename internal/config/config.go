package config

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	toml "github.com/pelletier/go-toml/v2"
)

type File struct {
	DefaultProfile string             `toml:"default_profile"`
	UI             UI                 `toml:"ui"`
	Agent          Agent              `toml:"agent"`
	Memory         Memory             `toml:"memory"`
	Profiles       map[string]Profile `toml:"profiles"`
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

func DefaultPath() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join("~", ".config", "otto", "config.toml")
	}
	return filepath.Join(home, ".config", "otto", "config.toml")
}

func Load(path string) (File, error) {
	file, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) && path == DefaultPath() {
			return File{}, nil
		}
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
