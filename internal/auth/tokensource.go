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
	config *oauth2.Config
	base   oauth2.TokenSource
	ctx    context.Context
	path   string

	mu          sync.Mutex
	creds       Credentials
	refreshDone chan struct{}
}

// TokenSource builds a refreshing, disk-persisting token source for the stored
// credentials. It refreshes via the refresh token when the access token has
// expired and saves the new tokens to path.
func (c Credentials) TokenSource(ctx context.Context, path string) oauth2.TokenSource {
	return newTokenSource(ctx, productionEndpoint(), c, path)
}

func newTokenSource(ctx context.Context, endpoint oauth2.Endpoint, creds Credentials, path string) oauth2.TokenSource {
	if ctx == nil {
		ctx = context.Background()
	}
	return &persistingSource{
		config: oauthConfig(endpoint, ""),
		ctx:    ctx,
		path:   path,
		creds:  creds,
	}
}

func (s *persistingSource) Token() (*oauth2.Token, error) {
	return s.TokenContext(s.ctx)
}

func (s *persistingSource) TokenContext(ctx context.Context) (*oauth2.Token, error) {
	if ctx == nil {
		ctx = context.Background()
	}
	for {
		s.mu.Lock()
		current := credentialsToken(s.creds)
		if current.Valid() {
			s.mu.Unlock()
			return current, nil
		}
		if done := s.refreshDone; done != nil {
			s.mu.Unlock()
			select {
			case <-done:
				continue
			case <-ctx.Done():
				return nil, ctx.Err()
			}
		}
		snapshot := s.creds
		done := make(chan struct{})
		s.refreshDone = done
		s.mu.Unlock()

		token, refreshed, err := s.refresh(ctx, snapshot)

		s.mu.Lock()
		if err == nil {
			s.creds = refreshed
		}
		close(done)
		s.refreshDone = nil
		s.mu.Unlock()

		if err != nil {
			return nil, err
		}
		return token, nil
	}
}

func (s *persistingSource) refresh(ctx context.Context, snapshot Credentials) (*oauth2.Token, Credentials, error) {
	refreshCtx, cancel := mergeTokenContext(s.ctx, ctx)
	defer cancel()
	refreshCtx = oauthHTTPContext(refreshCtx)

	source := s.base
	if source == nil {
		source = s.config.TokenSource(refreshCtx, credentialsToken(snapshot))
	}
	token, err := source.Token()
	if err != nil {
		if ctxErr := refreshCtx.Err(); ctxErr != nil {
			return nil, Credentials{}, ctxErr
		}
		return nil, Credentials{}, boundedAuthError(ErrAccessTokenRefreshFailed, err)
	}
	if ctxErr := refreshCtx.Err(); ctxErr != nil {
		return nil, Credentials{}, ctxErr
	}
	if token == nil {
		return nil, Credentials{}, ErrAccessTokenRefreshFailed
	}

	candidate := snapshot
	candidate.AccessToken = token.AccessToken
	candidate.Expiry = token.Expiry
	if token.RefreshToken != "" {
		candidate.RefreshToken = token.RefreshToken
	}
	if !credentialsWithinBounds(candidate) {
		return nil, Credentials{}, ErrAccessTokenRefreshFailed
	}
	changed := candidate.AccessToken != snapshot.AccessToken || candidate.RefreshToken != snapshot.RefreshToken || !candidate.Expiry.Equal(snapshot.Expiry)
	if changed {
		if err := candidate.Save(s.path); err != nil {
			return nil, Credentials{}, err
		}
	}
	return token, candidate, nil
}

func credentialsToken(creds Credentials) *oauth2.Token {
	return &oauth2.Token{
		AccessToken:  creds.AccessToken,
		RefreshToken: creds.RefreshToken,
		Expiry:       creds.Expiry,
	}
}

func mergeTokenContext(base, current context.Context) (context.Context, func()) {
	if current == nil {
		current = context.Background()
	}
	if base == nil {
		return current, func() {}
	}
	merged, cancel := context.WithCancel(current)
	stop := context.AfterFunc(base, cancel)
	return merged, func() {
		stop()
		cancel()
	}
}
