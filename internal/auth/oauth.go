package auth

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"fmt"
	"net"
	"net/http"
	"strings"

	"golang.org/x/oauth2"
)

// OAuth "Sign in with ChatGPT" constants. These mirror the OpenAI Codex CLI
// public client. Verify against openai/codex (codex-rs/login) if the flow
// changes.
const (
	clientID     = "app_EMoamEEZ73f0CkXaXp7hrann"
	issuer       = "https://auth.openai.com"
	authorizeURL = issuer + "/oauth/authorize"
	tokenURL     = issuer + "/oauth/token"
	oauthScopes  = "openid profile email offline_access"
)

// loopbackPorts are the redirect ports tried in order; matches the Codex CLI
// default and its fallback.
var loopbackPorts = []int{1455, 1457}

func productionEndpoint() oauth2.Endpoint {
	return oauth2.Endpoint{AuthURL: authorizeURL, TokenURL: tokenURL}
}

var oauthHTTPClientFactory = oauthHTTPClient

func oauthHTTPContext(ctx context.Context) context.Context {
	if ctx == nil {
		ctx = context.Background()
	}
	return context.WithValue(ctx, oauth2.HTTPClient, oauthHTTPClientFactory())
}

func oauthHTTPClient() *http.Client {
	return &http.Client{CheckRedirect: refuseOAuthRedirects}
}

func refuseOAuthRedirects(*http.Request, []*http.Request) error {
	return http.ErrUseLastResponse
}

func oauthConfig(endpoint oauth2.Endpoint, redirectURL string) *oauth2.Config {
	return &oauth2.Config{
		ClientID:    clientID,
		Endpoint:    endpoint,
		RedirectURL: redirectURL,
		Scopes:      strings.Fields(oauthScopes),
	}
}

// randomState returns a URL-safe random string for the OAuth state parameter.
func randomState() (string, error) {
	buf := make([]byte, 32)
	if _, err := rand.Read(buf); err != nil {
		return "", fmt.Errorf("generate state: %w", err)
	}
	return base64.RawURLEncoding.EncodeToString(buf), nil
}

// listenLoopback binds the first available port from the list on localhost.
func listenLoopback(ports []int) (net.Listener, int, error) {
	var lastErr error
	for _, port := range ports {
		ln, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", port))
		if err != nil {
			lastErr = err
			continue
		}
		return ln, ln.Addr().(*net.TCPAddr).Port, nil
	}
	return nil, 0, fmt.Errorf("no loopback port available (tried %v): %w", ports, lastErr)
}
