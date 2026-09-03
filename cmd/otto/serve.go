package main

import (
	"context"
	"errors"
	"io"
	"os"
	"os/signal"
	"syscall"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/session"
)

// maxServeListSessions mirrors internal/session's own list cap (unexported
// there), since serveFactories.list has no caller-supplied limit to pass through.
const maxServeListSessions = 20

// errSessionNotFound is returned by serveFactories.open for an id with no
// matching session in the workspace's session root.
var errSessionNotFound = errors.New("session not found")

// subscribeOSTerminate mirrors subscribeOSInterrupts but for SIGTERM, the
// signal used to ask a long-running otto serve process to shut down.
func subscribeOSTerminate() interruptSubscription {
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, syscall.SIGTERM)
	return interruptSubscription{
		signals: signals,
		stop:    func() { signal.Stop(signals) },
	}
}

// runServe is the entry point for `otto serve`, reached once the CLI has
// resolved an absolute socket path and confirmed dynamic content is
// available. It does not yet start internal/server's listener.
//
// ponytail: internal/server owns the accept loop and the SIGTERM-driven
// shutdown; subscribeTerminate is threaded through now so that wiring is a
// pure addition later, add the goroutine that reads from it when the real
// listener lands.
func runServe(ctx context.Context, socketPath string, subscribeTerminate func() interruptSubscription, stdout, stderr io.Writer) int {
	return fail(stderr, "serve: not wired (socket %s)", socketPath)
}

// serveFactories builds one app.Controller per server-side session, on top
// of the same replacement/runtime plumbing the CLI's /new and /resume use.
type serveFactories struct {
	create func(ctx context.Context) (*app.Controller, error)
	open   func(ctx context.Context, id string) (*app.Controller, error)
	list   func(ctx context.Context) (session.ListResult, error)
}

// serveFactories returns the create/open/list operations otto serve uses to
// back its per-session Controllers, all scoped to the already-resolved
// runtime. Callers only reach here once startup has confirmed dynamic
// content is available (see the options.serve check in runWithDependencies),
// so replacement building never needs to re-check the boundary itself.
func (b runtimeBuilder) serveFactories(runtime config.Runtime) serveFactories {
	return serveFactories{
		create: func(ctx context.Context) (*app.Controller, error) {
			replacement, err := b.freshReplacement(ctx, runtime)
			if err != nil {
				return nil, err
			}
			return b.controllerFromReplacement(replacement)
		},
		open: func(ctx context.Context, id string) (*app.Controller, error) {
			listed, err := session.List(ctx, b.sessionRoot, b.workspacePath, "", maxServeListSessions)
			if err != nil {
				return nil, err
			}
			for _, entry := range listed.Sessions {
				if entry.ID != id {
					continue
				}
				replacement, err := b.openReplacement(ctx, entry.Path)
				if err != nil {
					return nil, err
				}
				return b.controllerFromReplacement(replacement)
			}
			return nil, errSessionNotFound
		},
		list: func(ctx context.Context) (session.ListResult, error) {
			result, err := session.List(ctx, b.sessionRoot, b.workspacePath, "", maxServeListSessions)
			if errors.Is(err, os.ErrNotExist) {
				return session.ListResult{}, nil
			}
			return result, err
		},
	}
}

// controllerFromReplacement builds the Controller for a SessionReplacement
// that freshReplacement/openReplacement already returned successfully. Those
// two self-clean their own candidate session/runner on error, so only a
// failure here (which they never saw) needs to close them.
func (b runtimeBuilder) controllerFromReplacement(replacement app.SessionReplacement) (*app.Controller, error) {
	controller, err := b.newController(replacement.Session, replacement.Runner, replacement.RuntimeInfo, true)
	if err != nil {
		_ = closeRuntimeRunner(replacement.Runner)
		_ = replacement.Session.Close()
		return nil, err
	}
	return controller, nil
}
