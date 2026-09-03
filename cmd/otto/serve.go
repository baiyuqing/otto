package main

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/server"
	"github.com/baiyuqing/otto/internal/session"
)

// maxServeListSessions mirrors internal/session's own list cap (unexported
// there), since serveFactories.list has no caller-supplied limit to pass through.
const maxServeListSessions = 20

// errSessionNotFound is returned by serveFactories.open for an id with no
// matching session in the workspace's session root.
var errSessionNotFound = server.ErrSessionNotFound

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

func (b runtimeBuilder) runServe(ctx context.Context, runtime config.Runtime, socketPath string, subscribeTerminate func() interruptSubscription, stderr io.Writer) int {
	listener, err := server.Listen(socketPath)
	if err != nil {
		return fail(stderr, "serve: %v", b.redactError(err, &runtime))
	}
	defer listener.Close()

	serveCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	termination := subscribeTerminate()
	if termination.stop == nil {
		termination.stop = func() {}
	}
	defer termination.stop()
	terminated := make(chan struct{})
	go func() {
		defer close(terminated)
		select {
		case <-termination.signals:
			cancel()
		case <-serveCtx.Done():
		}
	}()

	factories := b.serveFactories(runtime)
	runtimeInfo := b.runtimeInfo(runtime)
	srv := server.New(serveCtx, server.Options{
		Create: factories.create,
		Open:   factories.open,
		List:   factories.list,
		Info: server.Info{
			Workspace: b.workspacePath,
			Provider:  runtimeInfo.Provider,
			Profile:   runtimeInfo.Profile,
			Model:     runtimeInfo.Model,
			Sandbox:   runtimeInfo.Sandbox.Summary(),
			Profiles:  b.profileNames(),
		},
		Logger: slog.New(slog.NewTextHandler(stderr, nil)),
	})
	serveErr := server.Serve(serveCtx, listener, srv)
	cancel()
	<-terminated
	closeErr := srv.Close()
	if serveErr != nil {
		return fail(stderr, "serve: %v", b.redactError(serveErr, &runtime))
	}
	if closeErr != nil {
		return fail(stderr, "serve: %v", b.redactError(closeErr, &runtime))
	}
	return 0
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
