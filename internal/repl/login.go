package repl

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
)

// Seam so tests can supply credentials without a browser or network.
var (
	replAuthLogin       = auth.Login
	errREPLLoginFailed  = errors.New("chatgpt sign-in failed")
	errREPLLogoutFailed = errors.New("stored chatgpt credentials could not be removed")
)

// loginCommand handles "/login" and "/login status". The OAuth flow runs
// inline: auth.Login blocks until the browser callback completes. On success it
// only saves credentials; newly written ChatGPT credentials are outside the
// immutable startup snapshot, so using them requires restarting Otto.
func (r *REPL) loginCommand(ctx context.Context, args string) (bool, error) {
	switch args {
	case "status":
		path, ok := r.authPath(ctx)
		if !ok {
			_, _ = fmt.Fprintln(r.stdout, auth.ErrInteractiveUnavailable.Error())
			return false, nil
		}
		line, _ := auth.StatusLine(path)
		_, _ = fmt.Fprintln(r.stdout, line)
		return false, nil
	case "":
		path, ok := r.authPath(ctx)
		if !ok {
			_, _ = fmt.Fprintln(r.stdout, auth.ErrInteractiveUnavailable.Error())
			return false, nil
		}
		if provider := r.backend.Info().Provider; provider != config.ProviderChatGPT {
			_, _ = fmt.Fprintf(r.stdout, "The %s provider authenticates with an API key from the environment; there is nothing to sign in to.\n", provider)
			return false, nil
		}
		creds, err := replAuthLogin(ctx, r.browserOpener())
		if err != nil {
			if ctxErr := ctx.Err(); ctxErr != nil {
				return false, &commandError{command: "/login", err: ctxErr}
			}
			return false, &commandError{command: "/login", err: errREPLLoginFailed}
		}
		if err := creds.Save(path); err != nil {
			return false, &commandError{command: "/login", err: auth.ErrCredentialsPersistence}
		}
		_, _ = fmt.Fprintln(r.stdout, "Signed in to ChatGPT. Restart Otto to use the new credentials.")
		return false, nil
	default:
		_, _ = fmt.Fprintln(r.stderr, "usage: /login [status]")
		return false, nil
	}
}

func (r *REPL) logoutCommand(ctx context.Context) (bool, error) {
	path, ok := r.authPath(ctx)
	if !ok {
		_, _ = fmt.Fprintln(r.stdout, auth.ErrInteractiveUnavailable.Error())
		return false, nil
	}
	if err := os.Remove(path); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			_, _ = fmt.Fprintln(r.stdout, "Not signed in to ChatGPT.")
			return false, nil
		}
		return false, &commandError{command: "/logout", err: errREPLLogoutFailed}
	}
	_, _ = fmt.Fprintln(r.stdout, "Signed out of ChatGPT.")
	return false, nil
}

func (r *REPL) authPath(ctx context.Context) (string, bool) {
	if !app.BackendDynamicContentAvailable(r.backend) {
		return "", false
	}
	return auth.PathFromContext(ctx)
}

// browserOpener prints the authorization URL (the reliable path) and also tries
// to launch the default browser. A failed launch is not fatal: the printed URL
// lets the callback flow complete either way.
func (r *REPL) browserOpener() func(string) error {
	return func(url string) error {
		_, _ = fmt.Fprintf(r.stdout, "Open this URL to sign in:\n\n  %s\n\n", url)
		_ = exec.Command("open", url).Start()
		return nil
	}
}
