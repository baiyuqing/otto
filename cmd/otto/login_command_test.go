package main

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/auth"
)

func runAuth(t *testing.T, home string, args ...string) (int, string, string) {
	t.Helper()
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), args,
		strings.NewReader(""), &stdout, &stderr,
		testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	return code, stdout.String(), stderr.String()
}

func TestLoginStatusNotSignedIn(t *testing.T) {
	home := t.TempDir()
	code, stdout, _ := runAuth(t, home, "login", "--status")
	if code == 0 {
		t.Fatalf("code = 0, want nonzero when not signed in")
	}
	if !strings.Contains(stdout, "Not signed in") {
		t.Fatalf("stdout = %q, want it to mention not signed in", stdout)
	}
}

func TestLoginStatusSignedIn(t *testing.T) {
	home := t.TempDir()
	creds := auth.Credentials{AccountID: "acct-xyz", Expiry: time.Now().Add(time.Hour)}
	if err := creds.Save(auth.PathForHome(home)); err != nil {
		t.Fatal(err)
	}
	code, stdout, stderr := runAuth(t, home, "login", "--status")
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr)
	}
	if strings.Contains(stdout, "acct-xyz") || !strings.Contains(stdout, "Signed in to ChatGPT") {
		t.Fatalf("stdout = %q, want sign-in state without account id", stdout)
	}
}

func TestLogoutRemovesCredentials(t *testing.T) {
	home := t.TempDir()
	path := auth.PathForHome(home)
	creds := auth.Credentials{AccountID: "acct-xyz"}
	if err := creds.Save(path); err != nil {
		t.Fatal(err)
	}
	code, stdout, stderr := runAuth(t, home, "logout")
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("credential file still exists: %v", err)
	}
	if !strings.Contains(stdout, "Signed out") {
		t.Fatalf("stdout = %q, want it to confirm sign-out", stdout)
	}
}

func TestLogoutWhenNotSignedIn(t *testing.T) {
	home := t.TempDir()
	code, stdout, stderr := runAuth(t, home, "logout")
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr)
	}
	if !strings.Contains(stdout, "Not signed in") {
		t.Fatalf("stdout = %q, want it to report not signed in", stdout)
	}
}

func TestLoginSavesCredentials(t *testing.T) {
	home := t.TempDir()
	original := authLogin
	authLogin = func(_ context.Context, _ func(string) error) (auth.Credentials, error) {
		return auth.Credentials{
			AccessToken: "tok", RefreshToken: "ref", AccountID: "acct-new",
			Expiry: time.Now().Add(time.Hour),
		}, nil
	}
	defer func() { authLogin = original }()

	code, stdout, stderr := runAuth(t, home, "login")
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr)
	}
	saved, err := auth.Load(auth.PathForHome(home))
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if saved.AccountID != "acct-new" || saved.AccessToken != "tok" {
		t.Fatalf("saved credentials = %+v", saved)
	}
	if strings.Contains(stdout, "acct-new") || !strings.Contains(stdout, "Signed in to ChatGPT") {
		t.Fatalf("stdout = %q, want bounded sign-in confirmation", stdout)
	}
}

func TestLoginFailureIsBounded(t *testing.T) {
	home := t.TempDir()
	original := authLogin
	authLogin = func(_ context.Context, _ func(string) error) (auth.Credentials, error) {
		return auth.Credentials{}, errors.New("login secret failure")
	}
	defer func() { authLogin = original }()

	code, _, stderr := runAuth(t, home, "login")
	if code == 0 {
		t.Fatal("login unexpectedly succeeded")
	}
	if strings.Contains(stderr, "login secret failure") {
		t.Fatalf("stderr leaked login error: %q", stderr)
	}
}

func TestLogoutFailureIsBounded(t *testing.T) {
	home := t.TempDir()
	path := auth.PathForHome(home)
	if err := os.MkdirAll(path, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(path, "child"), []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}
	code, _, stderr := runAuth(t, home, "logout")
	if code == 0 {
		t.Fatal("logout unexpectedly succeeded")
	}
	if strings.Contains(stderr, path) || strings.Contains(stderr, "directory not empty") {
		t.Fatalf("stderr leaked remove detail: %q", stderr)
	}
}
