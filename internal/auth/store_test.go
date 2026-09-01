package auth

import (
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

func TestSaveThenLoadRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "nested", "chatgpt.json")
	want := Credentials{
		AccessToken:  "access",
		RefreshToken: "refresh",
		IDToken:      "id",
		AccountID:    "acct-1",
		Expiry:       time.Now().Add(time.Hour).UTC().Round(time.Second),
	}
	if err := want.Save(path); err != nil {
		t.Fatalf("Save: %v", err)
	}
	got, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if got != want {
		t.Fatalf("round trip mismatch\n got: %+v\nwant: %+v", got, want)
	}
}

func TestSaveUsesOwnerOnlyPermissions(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix permission model")
	}
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	if err := (Credentials{AccessToken: "a"}).Save(path); err != nil {
		t.Fatalf("Save: %v", err)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if perm := info.Mode().Perm(); perm != 0o600 {
		t.Fatalf("perm = %o, want 600", perm)
	}
}

func TestLoadMissingReturnsErrNoCredentials(t *testing.T) {
	_, err := Load(filepath.Join(t.TempDir(), "absent.json"))
	if !errors.Is(err, ErrNoCredentials) {
		t.Fatalf("err = %v, want ErrNoCredentials", err)
	}
}

func TestLoadMalformedReturnsFixedUnavailableError(t *testing.T) {
	const secret = "credential-parse-secret"
	path := filepath.Join(t.TempDir(), secret, "chatgpt.json")
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(`{"access_token":"`+secret), 0o600); err != nil {
		t.Fatal(err)
	}
	_, err := Load(path)
	if !errors.Is(err, ErrCredentialsUnavailable) {
		t.Fatalf("err = %v, want ErrCredentialsUnavailable", err)
	}
	if got := err.Error(); strings.Contains(got, secret) || strings.Contains(got, path) {
		t.Fatalf("Load() leaked malformed credential detail: %q", got)
	}
}

func TestSaveFailureReturnsFixedPersistenceError(t *testing.T) {
	const secret = "credentials-path-secret"
	parent := filepath.Join(t.TempDir(), secret)
	if err := os.WriteFile(parent, []byte("not a directory"), 0o600); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(parent, "chatgpt.json")
	err := (Credentials{AccessToken: "access"}).Save(path)
	if !errors.Is(err, ErrCredentialsPersistence) {
		t.Fatalf("err = %v, want ErrCredentialsPersistence", err)
	}
	if got := err.Error(); strings.Contains(got, secret) || strings.Contains(got, path) {
		t.Fatalf("Save() leaked path detail: %q", got)
	}
}

func TestDefaultPathEndsWithExpectedSuffix(t *testing.T) {
	path, err := DefaultPath()
	if err != nil {
		t.Fatalf("DefaultPath: %v", err)
	}
	suffix := filepath.Join(".otto", "auth", "chatgpt.json")
	if filepath.Base(path) != "chatgpt.json" || !strings.HasSuffix(path, suffix) {
		t.Fatalf("DefaultPath = %q, want suffix %q", path, suffix)
	}
}
