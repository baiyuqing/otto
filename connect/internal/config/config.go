// Package config reads connect.toml.
package config

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/BurntSushi/toml"
)

// Config is a validated connect.toml.
type Config struct {
	Agent    Agent
	Telegram *Telegram // nil when the [telegram] section is absent
	Feishu   *Feishu   // nil when the [feishu] section is absent
}

// Agent is the ACP agent the connector starts.
type Agent struct {
	Command   []string // default ["otto", "acp"]
	Workspace string   // absolute path; required
}

// Telegram is the [telegram] section.
type Telegram struct {
	TokenEnv string
	// Token is read from the TokenEnv environment variable by Load. It is
	// never read from the file.
	Token   string
	Chats   []string
	Senders []string
}

// Feishu is the [feishu] section.
type Feishu struct {
	AppID        string
	AppSecretEnv string
	// AppSecret is read from the AppSecretEnv environment variable by Load.
	// It is never read from the file.
	AppSecret string
	Domain    string // "feishu" or "lark"
	Chats     []string
	Senders   []string
}

// file is the decoding target. Fields not declared here are reported by
// MetaData.Undecoded, which is how secret keys placed in the file are
// rejected.
type file struct {
	Agent struct {
		Command   []string `toml:"command"`
		Workspace string   `toml:"workspace"`
	} `toml:"agent"`
	Telegram *struct {
		TokenEnv string   `toml:"token_env"`
		Chats    []string `toml:"chats"`
		Senders  []string `toml:"senders"`
	} `toml:"telegram"`
	Feishu *struct {
		AppID        string   `toml:"app_id"`
		AppSecretEnv string   `toml:"app_secret_env"`
		Domain       string   `toml:"domain"`
		Chats        []string `toml:"chats"`
		Senders      []string `toml:"senders"`
	} `toml:"feishu"`
}

// Load reads and validates path. Unknown keys, a relative workspace, an
// empty command, and a missing or empty secret environment variable are
// errors.
func Load(path string) (*Config, error) {
	var f file
	md, err := toml.DecodeFile(path, &f)
	if err != nil {
		return nil, fmt.Errorf("config %s: %w", path, err)
	}
	if un := md.Undecoded(); len(un) > 0 {
		keys := make([]string, len(un))
		for i, k := range un {
			keys[i] = k.String()
		}
		return nil, fmt.Errorf("config %s: unknown keys: %s", path, strings.Join(keys, ", "))
	}

	cmd := f.Agent.Command
	if !md.IsDefined("agent", "command") {
		cmd = []string{"otto", "acp"}
	}
	if len(cmd) == 0 || cmd[0] == "" {
		return nil, errors.New("config: [agent].command must be a non-empty array with a non-empty first element")
	}
	ws := f.Agent.Workspace
	if ws == "" {
		return nil, errors.New("config: [agent].workspace is required")
	}
	if !filepath.IsAbs(ws) {
		return nil, fmt.Errorf("config: [agent].workspace must be an absolute path, got %q", ws)
	}

	cfg := &Config{Agent: Agent{Command: cmd, Workspace: filepath.Clean(ws)}}
	if t := f.Telegram; t != nil {
		if t.TokenEnv == "" {
			return nil, errors.New("config: [telegram].token_env is required")
		}
		token := os.Getenv(t.TokenEnv)
		if token == "" {
			return nil, fmt.Errorf("config: environment variable %s (telegram token_env) is empty or unset", t.TokenEnv)
		}
		cfg.Telegram = &Telegram{TokenEnv: t.TokenEnv, Token: token, Chats: t.Chats, Senders: t.Senders}
	}
	if fs := f.Feishu; fs != nil {
		if fs.AppID == "" {
			return nil, errors.New("config: [feishu].app_id is required")
		}
		if fs.AppSecretEnv == "" {
			return nil, errors.New("config: [feishu].app_secret_env is required")
		}
		domain := fs.Domain
		if domain == "" {
			domain = "feishu"
		}
		if domain != "feishu" && domain != "lark" {
			return nil, fmt.Errorf(`config: [feishu].domain must be "feishu" or "lark", got %q`, fs.Domain)
		}
		secret := os.Getenv(fs.AppSecretEnv)
		if secret == "" {
			return nil, fmt.Errorf("config: environment variable %s (feishu app_secret_env) is empty or unset", fs.AppSecretEnv)
		}
		cfg.Feishu = &Feishu{AppID: fs.AppID, AppSecretEnv: fs.AppSecretEnv, AppSecret: secret, Domain: domain, Chats: fs.Chats, Senders: fs.Senders}
	}
	if cfg.Telegram == nil && cfg.Feishu == nil {
		return nil, errors.New("config: no platform configured (add a [telegram] or [feishu] section)")
	}
	return cfg, nil
}
