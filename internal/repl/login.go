package repl

import (
	"context"
	"errors"
	"fmt"
	"os/exec"

	"github.com/baiyuqing/otto/internal/app"
)

var replOpenBrowser = func(url string) {
	_ = exec.Command("open", url).Start()
}

func (r *REPL) loginCommand(ctx context.Context, args string) (bool, error) {
	if !app.BackendDynamicContentAvailable(r.backend) {
		_, _ = fmt.Fprintln(r.stdout, app.ErrAuthenticationUnavailable)
		return false, nil
	}
	authentication, ok := r.backend.(app.Authentication)
	if !ok {
		_, _ = fmt.Fprintln(r.stdout, app.ErrAuthenticationUnavailable)
		return false, nil
	}
	switch args {
	case "status":
		line, _ := authentication.Status(ctx)
		_, _ = fmt.Fprintln(r.stdout, line)
		return false, nil
	case "":
		err := authentication.Login(ctx, r.browserOpener())
		if err != nil {
			if ctxErr := ctx.Err(); ctxErr != nil {
				return false, &commandError{command: "/login", err: ctxErr}
			}
			if errors.Is(err, app.ErrAuthenticationUnsupported) {
				provider := r.backend.Info().Provider
				_, _ = fmt.Fprintf(r.stdout, "The %s provider authenticates with an API key from the environment; there is nothing to sign in to.\n", provider)
				return false, nil
			}
			if errors.Is(err, app.ErrAuthenticationUnavailable) {
				_, _ = fmt.Fprintln(r.stdout, app.ErrAuthenticationUnavailable)
				return false, nil
			}
			return false, &commandError{command: "/login", err: err}
		}
		_, _ = fmt.Fprintln(r.stdout, "Signed in to ChatGPT. Restart Otto to use the new credentials.")
		return false, nil
	default:
		_, _ = fmt.Fprintln(r.stderr, "usage: /login [status]")
		return false, nil
	}
}

func (r *REPL) logoutCommand(ctx context.Context) (bool, error) {
	if !app.BackendDynamicContentAvailable(r.backend) {
		_, _ = fmt.Fprintln(r.stdout, app.ErrAuthenticationUnavailable)
		return false, nil
	}
	authentication, ok := r.backend.(app.Authentication)
	if !ok {
		_, _ = fmt.Fprintln(r.stdout, app.ErrAuthenticationUnavailable)
		return false, nil
	}
	removed, err := authentication.Logout(ctx)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return false, &commandError{command: "/logout", err: ctxErr}
		}
		return false, &commandError{command: "/logout", err: err}
	}
	if !removed {
		_, _ = fmt.Fprintln(r.stdout, "Not signed in to ChatGPT.")
		return false, nil
	}
	_, _ = fmt.Fprintln(r.stdout, "Signed out of ChatGPT.")
	return false, nil
}

// browserOpener prints the authorization URL and tries to launch the default
// browser. A failed launch is not fatal because the URL remains visible.
func (r *REPL) browserOpener() func(string) error {
	return func(url string) error {
		_, _ = fmt.Fprintf(r.stdout, "Open this URL to sign in:\n\n  %s\n\n", url)
		replOpenBrowser(url)
		return nil
	}
}
