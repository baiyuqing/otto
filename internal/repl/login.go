package repl

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"

	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
)

// Seams so tests can supply credentials and a temp path without a browser,
// network, or the real home directory.
var (
	replAuthLogin = auth.Login
	replAuthPath  = auth.DefaultPath
)

// loginCommand handles "/login" and "/login status". The OAuth flow runs
// inline: auth.Login blocks until the browser callback completes. On success it
// only saves credentials; using the subscription still requires starting a
// session on the chatgpt provider.
func (r *REPL) loginCommand(ctx context.Context, args string) (bool, error) {
	path, err := replAuthPath()
	if err != nil {
		return false, &commandError{command: "/login", err: err}
	}
	switch args {
	case "status":
		line, _ := auth.StatusLine(path)
		_, _ = fmt.Fprintln(r.stdout, line)
		return false, nil
	case "":
		if provider := r.backend.Info().Provider; provider != config.ProviderChatGPT {
			_, _ = fmt.Fprintf(r.stdout, "The %s provider authenticates with an API key from the environment; there is nothing to sign in to.\n", provider)
			return false, nil
		}
		creds, err := replAuthLogin(ctx, r.browserOpener())
		if err != nil {
			return false, &commandError{command: "/login", err: err}
		}
		if err := creds.Save(path); err != nil {
			return false, &commandError{command: "/login", err: err}
		}
		_, _ = fmt.Fprintf(r.stdout, "Signed in to ChatGPT (account %s). Start a new session on the chatgpt provider to use it.\n", creds.AccountID)
		return false, nil
	default:
		_, _ = fmt.Fprintln(r.stderr, "usage: /login [status]")
		return false, nil
	}
}

func (r *REPL) logoutCommand() (bool, error) {
	path, err := replAuthPath()
	if err != nil {
		return false, &commandError{command: "/logout", err: err}
	}
	if err := os.Remove(path); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			_, _ = fmt.Fprintln(r.stdout, "Not signed in to ChatGPT.")
			return false, nil
		}
		return false, &commandError{command: "/logout", err: err}
	}
	_, _ = fmt.Fprintln(r.stdout, "Signed out of ChatGPT.")
	return false, nil
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
