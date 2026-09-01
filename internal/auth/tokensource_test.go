package auth

import (
	"context"
	"net/http"
	"net/http/httptest"
	"path/filepath"
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
