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

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/baiyuqing/otto/chat-connector/internal/bridge"
	"github.com/baiyuqing/otto/chat-connector/internal/config"
	"github.com/baiyuqing/otto/chat-connector/internal/manage"
	"github.com/baiyuqing/otto/chat-connector/internal/metrics"
	"github.com/baiyuqing/otto/chat-connector/internal/ottoconfig"
	"github.com/baiyuqing/otto/chat-connector/internal/state"
	"github.com/baiyuqing/otto/chat-connector/internal/telegram"
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

	m := metrics.New()
	platforms, access, secretVars := platformsFor(cfg, store)

	management, managementErr := manage.FromCommand(cfg.Agent.Command, home)
	if managementErr != nil {
		slog.Info("management unavailable", "reason", managementErr)
	}
	b := bridge.New(bridge.Options{
		Agent: agent.New(agent.Options{
			Command: cfg.Agent.Command,
			Address: cfg.Agent.Address,
			Token:   cfg.Agent.Token,
			Dir:     cfg.Agent.Workspace,
			// The agent and its tools do not need the bot token or app secret.
			Env:     withoutVars(os.Environ(), secretVars...),
			Metrics: m,
		}),
		Metrics:   m,
		Manage:    management,
		Platforms: platforms,
		Access:    access,
		Store:     store,
		Models:    modelSource{path: ottoconfig.Path(home)},
	})
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	serveMetrics(ctx, m, cfg.Metrics.Listen)
	names := make([]string, len(platforms))
	for i, p := range platforms {
		names[i] = p.Name()
	}
	mode := "process"
	if cfg.Agent.Address != "" {
		mode = "address"
	}
	slog.Info("connector_started", "platforms", strings.Join(names, ","), "agent_mode", mode, "metrics_listen", cfg.Metrics.Listen)
	err = b.Run(ctx)
	reason := "signal"
	if err != nil {
		reason = "error"
	}
	slog.Info("connector_stopping", "reason", reason)
	return err
}

// platformsFor builds the platform for every section present in cfg, the
// admission lists by platform name, and the names of the environment
// variables that hold their secrets. config.Load fails when no platform is
// configured.
func platformsFor(cfg *config.Config, store *state.Store) ([]bridge.Platform, map[string]bridge.Access, []string) {
	var platforms []bridge.Platform
	access := map[string]bridge.Access{}
	var secretVars []string
	if cfg.Agent.TokenEnv != "" {
		secretVars = append(secretVars, cfg.Agent.TokenEnv)
	}
	if t := cfg.Telegram; t != nil {
		bot := telegram.New(t.Token, store, t.Chats...)
		platforms = append(platforms, bot)
		access[bot.Name()] = bridge.Access{Chats: t.Chats, Senders: t.Senders}
		secretVars = append(secretVars, t.TokenEnv)
	}
	return platforms, access, secretVars
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

// modelSource adapts the local Otto configuration to the bridge's model
// picker. Every "/model" re-reads it, so an edit on the host shows up without
// restarting the connector.
type modelSource struct{ path string }

func (s modelSource) Models() (bridge.ModelSnapshot, error) {
	snapshot, err := ottoconfig.Read(s.path)
	if err != nil {
		return bridge.ModelSnapshot{}, err
	}
	out := bridge.ModelSnapshot{
		Default: snapshot.Default,
		Models:  make([]string, 0, len(snapshot.Models)),
	}
	for _, model := range snapshot.Models {
		out.Models = append(out.Models, model.Selector)
	}
	return out, nil
}

// serveMetrics starts the metrics endpoint. A bind failure is logged and the
// connector runs without the endpoint.
func serveMetrics(ctx context.Context, m *metrics.Metrics, addr string) {
	if addr == "" {
		return
	}
	if err := m.Serve(ctx, addr); err != nil {
		slog.Warn("metrics_listen_failed", "address", addr, "error", err)
	}
}
