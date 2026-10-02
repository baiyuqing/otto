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

	// config.Load fails when no platform is configured.
	t := cfg.Telegram
	bot := telegram.New(t.Token, store)
	platforms := []bridge.Platform{bot}
	access := map[string]bridge.Access{bot.Name(): {Chats: t.Chats, Senders: t.Senders}}

	b := bridge.New(bridge.Options{
		Agent: agent.New(agent.Options{
			Command: cfg.Agent.Command,
			Dir:     cfg.Agent.Workspace,
			// The agent and its tools do not need the bot token.
			Env: withoutVars(os.Environ(), t.TokenEnv),
		}),
		Platforms: platforms,
		Access:    access,
		Store:     store,
	})
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	return b.Run(ctx)
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
