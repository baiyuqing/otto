package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"

	"github.com/baiyuqing/otto/internal/auth"
)

// authLogin is the OAuth login entry point, a seam so tests can supply
// credentials without a browser or network.
var authLogin = auth.Login

// runAuthCommand handles "otto login [--status]" and "otto logout", dispatched
// before the main flag set is parsed. It reads and writes only the credential
// file; it builds no provider, session, or controller.
func runAuthCommand(ctx context.Context, args []string, stdout, stderr io.Writer, lookup environmentLookup) int {
	command := args[0]
	rest := args[1:]

	flags := flag.NewFlagSet("otto "+command, flag.ContinueOnError)
	flags.SetOutput(stderr)
	status := flags.Bool("status", false, "report sign-in status without signing in")
	if err := flags.Parse(rest); err != nil {
		return 2
	}

	home, err := resolveHome(lookup, currentOSUserHome)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	path := auth.PathForHome(home)

	switch command {
	case "logout":
		return runLogout(path, stdout, stderr)
	case "login":
		if *status {
			return runLoginStatus(path, stdout)
		}
		return runLogin(ctx, path, stdout, stderr)
	default:
		return fail(stderr, "unknown command %q", command)
	}
}

func runLoginStatus(path string, stdout io.Writer) int {
	creds, err := auth.Load(path)
	if err != nil {
		_, _ = fmt.Fprintln(stdout, "Not signed in to ChatGPT. Run 'otto login'.")
		return 1
	}
	_, _ = fmt.Fprintf(stdout, "Signed in to ChatGPT (account %s).\n", creds.AccountID)
	if !creds.Expiry.IsZero() {
		_, _ = fmt.Fprintf(stdout, "Access token expires: %s\n", creds.Expiry.Format("2006-01-02 15:04:05 MST"))
	}
	return 0
}

func runLogout(path string, stdout, stderr io.Writer) int {
	if err := os.Remove(path); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			_, _ = fmt.Fprintln(stdout, "Not signed in to ChatGPT.")
			return 0
		}
		return fail(stderr, "remove credentials: %v", err)
	}
	_, _ = fmt.Fprintln(stdout, "Signed out of ChatGPT.")
	return 0
}

func runLogin(ctx context.Context, path string, stdout, stderr io.Writer) int {
	creds, err := authLogin(ctx, browserOpener(stdout))
	if err != nil {
		return fail(stderr, "login: %v", err)
	}
	if err := creds.Save(path); err != nil {
		return fail(stderr, "save credentials: %v", err)
	}
	_, _ = fmt.Fprintf(stdout, "Signed in to ChatGPT (account %s).\n", creds.AccountID)
	return 0
}

// browserOpener prints the authorization URL (the reliable path) and also tries
// to launch the default browser. A failed launch is not fatal: the printed URL
// is the fallback, so the callback flow can still complete.
func browserOpener(stdout io.Writer) func(string) error {
	return func(url string) error {
		_, _ = fmt.Fprintf(stdout, "Open this URL to sign in:\n\n  %s\n\n", url)
		_ = exec.Command("open", url).Start()
		return nil
	}
}
