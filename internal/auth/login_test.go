package auth

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"

	"golang.org/x/oauth2"
)

func TestLoginExchangesCodeAndExtractsAccountID(t *testing.T) {
	// Use an ephemeral loopback port so the test does not depend on 1455.
	restore := loopbackPorts
	loopbackPorts = []int{0}
	defer func() { loopbackPorts = restore }()

	idToken := fakeIDToken(t, map[string]any{
		"https://api.openai.com/auth": map[string]any{"chatgpt_account_id": "acct-xyz"},
	})

	var gotVerifier, gotRedirect string
	tokenServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = r.ParseForm()
		if got := r.Form.Get("grant_type"); got != "authorization_code" {
			t.Errorf("grant_type = %q, want authorization_code", got)
		}
		if got := r.Form.Get("code"); got != "the-code" {
			t.Errorf("code = %q, want the-code", got)
		}
		gotVerifier = r.Form.Get("code_verifier")
		gotRedirect = r.Form.Get("redirect_uri")
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, `{"access_token":"acc","refresh_token":"ref","token_type":"bearer","expires_in":3600,"id_token":%q}`, idToken)
	}))
	defer tokenServer.Close()

	endpoint := oauth2.Endpoint{AuthURL: "https://auth.example/authorize", TokenURL: tokenServer.URL}

	// open() stands in for the browser: it parses the authorize URL and drives
	// the loopback callback with a fixed code and the flow's own state value.
	open := func(rawAuthURL string) error {
		parsed, err := url.Parse(rawAuthURL)
		if err != nil {
			return err
		}
		q := parsed.Query()
		if q.Get("code_challenge_method") != "S256" {
			t.Errorf("code_challenge_method = %q, want S256", q.Get("code_challenge_method"))
		}
		redirect := q.Get("redirect_uri")
		state := q.Get("state")
		go func() {
			cbURL := fmt.Sprintf("%s?code=the-code&state=%s", redirect, url.QueryEscape(state))
			resp, err := http.Get(cbURL)
			if err == nil {
				resp.Body.Close()
			}
		}()
		return nil
	}

	creds, err := login(context.Background(), endpoint, open)
	if err != nil {
		t.Fatalf("login: %v", err)
	}
	if creds.AccountID != "acct-xyz" {
		t.Fatalf("account id = %q, want acct-xyz", creds.AccountID)
	}
	if creds.AccessToken != "acc" || creds.RefreshToken != "ref" {
		t.Fatalf("unexpected tokens: %+v", creds)
	}
	if gotVerifier == "" {
		t.Fatal("PKCE code_verifier not sent on exchange")
	}
	if gotRedirect == "" {
		t.Fatal("redirect_uri not sent on exchange")
	}
}

func TestLoginRejectsStateMismatch(t *testing.T) {
	restore := loopbackPorts
	loopbackPorts = []int{0}
	defer func() { loopbackPorts = restore }()

	tokenServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Errorf("token endpoint must not be reached on state mismatch")
	}))
	defer tokenServer.Close()
	endpoint := oauth2.Endpoint{AuthURL: "https://auth.example/authorize", TokenURL: tokenServer.URL}

	open := func(rawAuthURL string) error {
		parsed, _ := url.Parse(rawAuthURL)
		redirect := parsed.Query().Get("redirect_uri")
		go func() {
			resp, err := http.Get(redirect + "?code=x&state=WRONG")
			if err == nil {
				resp.Body.Close()
			}
		}()
		return nil
	}

	if _, err := login(context.Background(), endpoint, open); err == nil {
		t.Fatal("expected error on state mismatch")
	}
}
