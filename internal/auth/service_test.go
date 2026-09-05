package auth

import (
	"context"
	"errors"
	"path/filepath"
	"strings"
	"testing"
)

func TestServiceLoginPersistsCredentialsAndHidesOAuthError(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	service := NewService(path)
	service.login = func(context.Context, func(string) error) (Credentials, error) {
		return Credentials{AccessToken: "secret", AccountID: "acct-1"}, errors.New("oauth secret")
	}
	err := service.Login(context.Background(), func(string) error { return nil })
	if !errors.Is(err, ErrLoginFailed) {
		t.Fatalf("Login() error = %v, want ErrLoginFailed", err)
	}
	if strings.Contains(errString(err), "oauth secret") {
		t.Fatal("Login() leaked OAuth error")
	}
}

func TestServiceLoginSavesAndLogoutReportsPresence(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	service := NewService(path)
	service.login = func(context.Context, func(string) error) (Credentials, error) {
		return Credentials{AccessToken: "secret", AccountID: "acct-2"}, nil
	}
	if err := service.Login(context.Background(), func(string) error { return nil }); err != nil {
		t.Fatal(err)
	}
	line, signedIn := service.Status(context.Background())
	if !signedIn || !strings.Contains(line, "Signed in to ChatGPT") || strings.Contains(line, "acct-2") {
		t.Fatalf("Status() = %q, %v", line, signedIn)
	}
	removed, err := service.Logout(context.Background())
	if err != nil || !removed {
		t.Fatalf("Logout() = %v, %v", removed, err)
	}
	removed, err = service.Logout(context.Background())
	if err != nil || removed {
		t.Fatalf("missing Logout() = %v, %v", removed, err)
	}
}

func TestServicePreservesCancellation(t *testing.T) {
	service := NewService(filepath.Join(t.TempDir(), "chatgpt.json"))
	service.login = func(ctx context.Context, _ func(string) error) (Credentials, error) {
		return Credentials{}, ctx.Err()
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := service.Login(ctx, nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("Login() error = %v, want context.Canceled", err)
	}
}

func TestServiceDoesNotSaveAfterOAuthCancellation(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	service := NewService(path)
	ctx, cancel := context.WithCancel(context.Background())
	service.login = func(context.Context, func(string) error) (Credentials, error) {
		cancel()
		return Credentials{AccessToken: "secret"}, nil
	}
	if err := service.Login(ctx, func(string) error { return nil }); !errors.Is(err, context.Canceled) {
		t.Fatalf("Login() error = %v, want context.Canceled", err)
	}
	if _, err := Load(path); !errors.Is(err, ErrNoCredentials) {
		t.Fatalf("credentials saved after cancellation: %v", err)
	}
}

func errString(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}
