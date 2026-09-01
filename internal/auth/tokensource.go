package auth

import (
	"context"
	"sync"

	"golang.org/x/oauth2"
)

// persistingSource wraps the refreshing token source returned by
// oauth2.Config.TokenSource and writes rotated tokens back to disk so a
// refreshed access/refresh token survives across processes.
type persistingSource struct {
	base  oauth2.TokenSource
	ctx   context.Context
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
		ctx:   ctx,
		path:  path,
		creds: creds,
	}
}

func (s *persistingSource) Token() (*oauth2.Token, error) {
	tok, err := s.base.Token()
	if err != nil {
		if ctxErr := s.ctx.Err(); ctxErr != nil {
			return nil, ctxErr
		}
		return nil, boundedAuthError(ErrAccessTokenRefreshFailed, err)
	}
	if ctxErr := s.ctx.Err(); ctxErr != nil {
		return nil, ctxErr
	}
	if tok == nil {
		return nil, ErrAccessTokenRefreshFailed
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	candidate := s.creds
	candidate.AccessToken = tok.AccessToken
	candidate.Expiry = tok.Expiry
	if tok.RefreshToken != "" {
		candidate.RefreshToken = tok.RefreshToken
	}
	if !credentialsWithinBounds(candidate) {
		return nil, ErrAccessTokenRefreshFailed
	}
	changed := candidate.AccessToken != s.creds.AccessToken || candidate.RefreshToken != s.creds.RefreshToken || !candidate.Expiry.Equal(s.creds.Expiry)
	if changed {
		if err := candidate.Save(s.path); err != nil {
			return nil, err
		}
		s.creds = candidate
	}
	return tok, nil
}
