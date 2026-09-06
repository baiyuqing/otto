package repl

import (
	"context"
	"fmt"
	"strings"

	"github.com/baiyuqing/otto/internal/app"
)

// sandboxCommand shows the current sandbox state, or re-reads the [sandbox]
// configuration and applies it to the running process.
func (r *REPL) sandboxCommand(ctx context.Context, args string) (bool, error) {
	switch strings.TrimSpace(args) {
	case "":
		r.printSandbox(r.backend.Info().Sandbox)
		return false, nil
	case "reload":
		reloader, ok := r.backend.(app.SandboxReloader)
		if !ok {
			return false, &commandError{command: "/sandbox", err: app.ErrSandboxReloadUnavailable}
		}
		info, err := reloader.ReloadSandbox(ctx)
		if err != nil {
			return false, &commandError{command: "/sandbox", err: err}
		}
		r.printSandbox(info)
		return false, nil
	}
	_, _ = fmt.Fprintf(r.stderr, "unknown command: /sandbox %s\n", args)
	return false, nil
}

func (r *REPL) printSandbox(info app.SandboxInfo) {
	_, _ = fmt.Fprintf(r.stdout, "Sandbox: %s\n", info.Summary())
	if reason := info.ReasonCode(); reason != "" {
		_, _ = fmt.Fprintf(r.stdout, "Sandbox reason: %s\n", reason)
	}
}
