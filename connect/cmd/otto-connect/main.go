// Command otto-connect connects chat platforms to an ACP agent (by default
// `otto acp`). See docs/specs/2026-10-02-otto-connect.md.
package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"syscall"

	"github.com/baiyuqing/otto/connect/internal/agent"
	"github.com/baiyuqing/otto/connect/internal/bridge"
	"github.com/baiyuqing/otto/connect/internal/config"
	"github.com/baiyuqing/otto/connect/internal/feishu"
	"github.com/baiyuqing/otto/connect/internal/state"
	"github.com/baiyuqing/otto/connect/internal/telegram"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "otto-connect:", err)
		os.Exit(1)
	}
}

func run() error {
	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}
	configPath := flag.String("config", filepath.Join(home, ".config", "otto", "connect.toml"), "path of connect.toml")
	flag.Parse()
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, nil)))

	cfg, err := config.Load(*configPath)
	if err != nil {
		return err
	}
	store, err := state.Open(filepath.Join(home, ".otto", "connect", "state.json"))
	if err != nil {
		return err
	}

	platforms, access, secretVars, err := platformsFor(cfg, store)
	if err != nil {
		return err
	}

	b := bridge.New(bridge.Options{
		Agent: agent.New(agent.Options{
			Command: cfg.Agent.Command,
			Dir:     cfg.Agent.Workspace,
			// The agent and its tools do not need the bot token or app secret.
			Env: withoutVars(os.Environ(), secretVars...),
		}),
		Platforms: platforms,
		Access:    access,
		Store:     store,
	})
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	return b.Run(ctx)
}

// platformsFor builds the platform for every section present in cfg, the
// admission lists by platform name, and the names of the environment
// variables that hold their secrets. config.Load fails when no platform is
// configured.
func platformsFor(cfg *config.Config, store *state.Store) ([]bridge.Platform, map[string]bridge.Access, []string, error) {
	var platforms []bridge.Platform
	access := map[string]bridge.Access{}
	var secretVars []string
	if t := cfg.Telegram; t != nil {
		bot := telegram.New(t.Token, store)
		platforms = append(platforms, bot)
		access[bot.Name()] = bridge.Access{Chats: t.Chats, Senders: t.Senders}
		secretVars = append(secretVars, t.TokenEnv)
	}
	if f := cfg.Feishu; f != nil {
		p, err := feishu.New(feishu.Options{AppID: f.AppID, AppSecret: f.AppSecret, Domain: f.Domain, Chats: f.Chats, Senders: f.Senders})
		if err != nil {
			return nil, nil, nil, err
		}
		platforms = append(platforms, p)
		access[p.Name()] = bridge.Access{Chats: f.Chats, Senders: f.Senders}
		secretVars = append(secretVars, f.AppSecretEnv)
	}
	return platforms, access, secretVars, nil
}

// withoutVars returns env without the entries named in names.
func withoutVars(env []string, names ...string) []string {
	out := make([]string, 0, len(env))
	for _, e := range env {
		name, _, _ := strings.Cut(e, "=")
		if !slices.Contains(names, name) {
			out = append(out, e)
		}
	}
	return out
}
