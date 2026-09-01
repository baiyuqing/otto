package auth

import (
	"context"
	"fmt"
	"sync"

	"golang.org/x/oauth2"
)

// persistingSource wraps the refreshing token source returned by
// oauth2.Config.TokenSource and writes rotated tokens back to disk so a
// refreshed access/refresh token survives across processes.
type persistingSource struct {
	base  oauth2.TokenSource
	path  string
	mu    sync.Mutex
	creds Credentials
}

// TokenSource builds a refreshing, disk-persisting token source for the stored
// credentials. It refreshes via the refresh token when the access token has
// expired and saves the new tokens to path.
func (c Credentials) TokenSource(ctx context.Context, path string) oauth2.TokenSource {
	return newTokenSource(ctx, productionEndpoint(), c, path)
}

func newTokenSource(ctx context.Context, endpoint oauth2.Endpoint, creds Credentials, path string) oauth2.TokenSource {
	config := oauthConfig(endpoint, "")
	tok := &oauth2.Token{
		AccessToken:  creds.AccessToken,
		RefreshToken: creds.RefreshToken,
		Expiry:       creds.Expiry,
	}
	return &persistingSource{
		base:  config.TokenSource(ctx, tok),
		path:  path,
		creds: creds,
	}
}

func (s *persistingSource) Token() (*oauth2.Token, error) {
	tok, err := s.base.Token()
	if err != nil {
		return nil, fmt.Errorf("obtain access token: %w", err)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	changed := tok.AccessToken != s.creds.AccessToken || !tok.Expiry.Equal(s.creds.Expiry)
	if tok.RefreshToken != "" && tok.RefreshToken != s.creds.RefreshToken {
		s.creds.RefreshToken = tok.RefreshToken
		changed = true
	}
	if changed {
		s.creds.AccessToken = tok.AccessToken
		s.creds.Expiry = tok.Expiry
		if err := s.creds.Save(s.path); err != nil {
			return nil, fmt.Errorf("persist refreshed token: %w", err)
		}
	}
	return tok, nil
}
