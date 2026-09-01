package auth

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/oauth2"
)

func TestTokenSourceRefreshesAndPersists(t *testing.T) {
	var calls int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&calls, 1)
		if err := r.ParseForm(); err != nil {
			t.Errorf("parse form: %v", err)
		}
		if got := r.Form.Get("grant_type"); got != "refresh_token" {
			t.Errorf("grant_type = %q, want refresh_token", got)
		}
		if got := r.Form.Get("refresh_token"); got != "refresh-old" {
			t.Errorf("refresh_token = %q, want refresh-old", got)
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"access_token":"access-new","refresh_token":"refresh-new","token_type":"bearer","expires_in":3600}`))
	}))
	defer server.Close()

	path := filepath.Join(t.TempDir(), "chatgpt.json")
	creds := Credentials{
		AccessToken:  "access-old",
		RefreshToken: "refresh-old",
		AccountID:    "acct-1",
		Expiry:       time.Now().Add(-time.Minute), // expired → forces refresh
	}
	src := newTokenSource(context.Background(), oauth2.Endpoint{TokenURL: server.URL}, creds, path)

	tok, err := src.Token()
	if err != nil {
		t.Fatalf("Token: %v", err)
	}
	if tok.AccessToken != "access-new" {
		t.Fatalf("access token = %q, want access-new", tok.AccessToken)
	}

	saved, err := Load(path)
	if err != nil {
		t.Fatalf("Load persisted: %v", err)
	}
	if saved.AccessToken != "access-new" || saved.RefreshToken != "refresh-new" {
		t.Fatalf("persisted tokens not rotated: %+v", saved)
	}
	if saved.AccountID != "acct-1" {
		t.Fatalf("account id lost on refresh: %q", saved.AccountID)
	}
	if atomic.LoadInt32(&calls) != 1 {
		t.Fatalf("token endpoint called %d times, want 1", calls)
	}
}

func TestTokenSourceValidTokenSkipsRefresh(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Errorf("token endpoint should not be called for a valid token")
	}))
	defer server.Close()

	path := filepath.Join(t.TempDir(), "chatgpt.json")
	creds := Credentials{
		AccessToken:  "access-valid",
		RefreshToken: "refresh",
		Expiry:       time.Now().Add(time.Hour),
	}
	src := newTokenSource(context.Background(), oauth2.Endpoint{TokenURL: server.URL}, creds, path)
	tok, err := src.Token()
	if err != nil {
		t.Fatalf("Token: %v", err)
	}
	if tok.AccessToken != "access-valid" {
		t.Fatalf("access token = %q, want access-valid", tok.AccessToken)
	}
}

func TestTokenSourceRefreshFailureReturnsFixedError(t *testing.T) {
	const (
		accessToken  = "access-secret"
		refreshToken = "refresh-secret"
		accountID    = "acct-secret"
	)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadRequest)
		_, _ = w.Write([]byte(`{"error":"` + accessToken + ` ` + refreshToken + ` ` + accountID + `"}`))
	}))
	defer server.Close()

	path := filepath.Join(t.TempDir(), accountID, "chatgpt.json")
	src := newTokenSource(context.Background(), oauth2.Endpoint{TokenURL: server.URL}, Credentials{
		AccessToken: accessToken, RefreshToken: refreshToken, AccountID: accountID, Expiry: time.Now().Add(-time.Minute),
	}, path)
	_, err := src.Token()
	if !errors.Is(err, ErrAccessTokenRefreshFailed) {
		t.Fatalf("err = %v, want ErrAccessTokenRefreshFailed", err)
	}
	for _, secret := range []string{accessToken, refreshToken, accountID, path} {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("Token() leaked %q: %v", secret, err)
		}
	}
}

func TestTokenSourceSaveFailureReturnsFixedError(t *testing.T) {
	const (
		accessToken  = "access-secret"
		refreshToken = "refresh-secret"
		accountID    = "acct-secret"
	)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"access_token":"access-rotated","refresh_token":"refresh-rotated","token_type":"bearer","expires_in":3600}`))
	}))
	defer server.Close()

	parent := filepath.Join(t.TempDir(), accountID)
	if err := os.WriteFile(parent, []byte("not a directory"), 0o600); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(parent, "chatgpt.json")
	src := newTokenSource(context.Background(), oauth2.Endpoint{TokenURL: server.URL}, Credentials{
		AccessToken: accessToken, RefreshToken: refreshToken, AccountID: accountID, Expiry: time.Now().Add(-time.Minute),
	}, path)
	_, err := src.Token()
	if !errors.Is(err, ErrCredentialsPersistence) {
		t.Fatalf("err = %v, want ErrCredentialsPersistence", err)
	}
	for _, secret := range []string{accessToken, refreshToken, accountID, path} {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("Token() leaked %q: %v", secret, err)
		}
	}
}

func TestTokenSourceRefreshFailureDoesNotInspectArbitraryError(t *testing.T) {
	hostile := &hostileAuthError{}
	source := &persistingSource{
		base:  tokenSourceFunc(func() (*oauth2.Token, error) { return nil, hostile }),
		ctx:   context.Background(),
		path:  filepath.Join(t.TempDir(), "chatgpt.json"),
		creds: Credentials{AccessToken: "access-old", RefreshToken: "refresh-old", AccountID: "acct-1", Expiry: time.Now().Add(-time.Minute)},
	}
	_, err := source.Token()
	if !errors.Is(err, ErrAccessTokenRefreshFailed) {
		t.Fatalf("err = %v, want ErrAccessTokenRefreshFailed", err)
	}
	if hostile.calls() != 0 {
		t.Fatalf("hostile auth error methods called %d times", hostile.calls())
	}
	if errors.Unwrap(err) != nil {
		t.Fatalf("errors.Unwrap(err) = %#v, want nil", errors.Unwrap(err))
	}
}

func TestTokenSourceRejectsOversizedRotatedCredentialsWithoutPersisting(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	source := &persistingSource{
		base: tokenSourceFunc(func() (*oauth2.Token, error) {
			return &oauth2.Token{AccessToken: strings.Repeat("x", maxCredentialFileBytes+1), RefreshToken: "refresh-new", Expiry: time.Now().Add(time.Hour)}, nil
		}),
		ctx:  context.Background(),
		path: path,
		creds: Credentials{
			AccessToken: "access-old", RefreshToken: "refresh-old", AccountID: "acct-1", Expiry: time.Now().Add(-time.Minute),
		},
	}
	_, err := source.Token()
	if !errors.Is(err, ErrAccessTokenRefreshFailed) {
		t.Fatalf("err = %v, want ErrAccessTokenRefreshFailed", err)
	}
	if source.creds.AccessToken != "access-old" || source.creds.RefreshToken != "refresh-old" {
		t.Fatalf("oversized refresh mutated retained credentials: %+v", source.creds)
	}
	if _, statErr := os.Stat(path); !os.IsNotExist(statErr) {
		t.Fatalf("oversized refresh persisted credential file: %v", statErr)
	}
}

func TestTokenSourcePreservesContextCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	src := newTokenSource(ctx, oauth2.Endpoint{TokenURL: "http://127.0.0.1:1"}, Credentials{
		AccessToken: "access", RefreshToken: "refresh", Expiry: time.Now().Add(-time.Minute),
	}, filepath.Join(t.TempDir(), "chatgpt.json"))
	_, err := src.Token()
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("err = %v, want context.Canceled", err)
	}
}

type tokenSourceFunc func() (*oauth2.Token, error)

func (f tokenSourceFunc) Token() (*oauth2.Token, error) { return f() }

type hostileAuthError struct{ callsCount atomic.Int32 }

func (e *hostileAuthError) Error() string {
	e.callsCount.Add(1)
	return "hostile auth error"
}

func (e *hostileAuthError) Is(error) bool {
	e.callsCount.Add(1)
	return false
}

func (e *hostileAuthError) Unwrap() error {
	e.callsCount.Add(1)
	return nil
}

func (e *hostileAuthError) calls() int { return int(e.callsCount.Load()) }
