package auth

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"time"

	"golang.org/x/oauth2"
)

// Login runs the "Sign in with ChatGPT" OAuth PKCE flow: it starts a loopback
// callback server, invokes open with the authorization URL (typically to launch
// a browser), waits for the redirect, exchanges the code, and returns the
// resulting credentials. The caller is responsible for persisting them.
func Login(ctx context.Context, open func(url string) error) (Credentials, error) {
	return login(ctx, productionEndpoint(), open)
}

type callbackResult struct {
	code string
	err  error
}

var errAuthorizationCodeExchangeFailed = errors.New("chatgpt authorization code exchange failed")

func login(ctx context.Context, endpoint oauth2.Endpoint, open func(url string) error) (Credentials, error) {
	listener, port, err := listenLoopback(loopbackPorts)
	if err != nil {
		return Credentials{}, err
	}
	defer listener.Close()

	redirectURL := fmt.Sprintf("http://localhost:%d/auth/callback", port)
	config := oauthConfig(endpoint, redirectURL)

	verifier := oauth2.GenerateVerifier()
	state, err := randomState()
	if err != nil {
		return Credentials{}, err
	}
	authURL := config.AuthCodeURL(state,
		oauth2.AccessTypeOffline,
		oauth2.S256ChallengeOption(verifier),
	)

	results := make(chan callbackResult, 1)
	server := &http.Server{Handler: callbackHandler(state, results)}
	go func() {
		if serveErr := server.Serve(listener); serveErr != nil && !errors.Is(serveErr, http.ErrServerClosed) {
			results <- callbackResult{err: fmt.Errorf("callback server: %w", serveErr)}
		}
	}()
	defer func() {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		_ = server.Shutdown(shutdownCtx)
	}()

	if err := open(authURL); err != nil {
		return Credentials{}, fmt.Errorf("open authorization URL: %w", err)
	}

	var code string
	select {
	case <-ctx.Done():
		return Credentials{}, ctx.Err()
	case res := <-results:
		if res.err != nil {
			return Credentials{}, res.err
		}
		code = res.code
	}

	exchangeCtx := oauthHTTPContext(ctx)
	token, err := config.Exchange(exchangeCtx, code, oauth2.VerifierOption(verifier))
	if err != nil {
		if ctxErr := exchangeCtx.Err(); ctxErr != nil {
			return Credentials{}, ctxErr
		}
		return Credentials{}, errAuthorizationCodeExchangeFailed
	}
	if ctxErr := exchangeCtx.Err(); ctxErr != nil {
		return Credentials{}, ctxErr
	}
	idToken, _ := token.Extra("id_token").(string)
	if idToken == "" {
		return Credentials{}, errors.New("token response missing id_token")
	}
	accountID, err := accountIDFromIDToken(idToken)
	if err != nil {
		return Credentials{}, err
	}
	return Credentials{
		AccessToken:  token.AccessToken,
		RefreshToken: token.RefreshToken,
		IDToken:      idToken,
		AccountID:    accountID,
		Expiry:       token.Expiry,
	}, nil
}

func callbackHandler(wantState string, results chan<- callbackResult) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/auth/callback", func(w http.ResponseWriter, r *http.Request) {
		query := r.URL.Query()
		if errParam := query.Get("error"); errParam != "" {
			writeBrowserMessage(w, "Sign-in failed. You can close this tab.")
			results <- callbackResult{err: fmt.Errorf("authorization error: %s", errParam)}
			return
		}
		if query.Get("state") != wantState {
			writeBrowserMessage(w, "Sign-in failed (state mismatch). You can close this tab.")
			results <- callbackResult{err: errors.New("state mismatch on callback")}
			return
		}
		code := query.Get("code")
		if code == "" {
			writeBrowserMessage(w, "Sign-in failed (missing code). You can close this tab.")
			results <- callbackResult{err: errors.New("callback missing authorization code")}
			return
		}
		writeBrowserMessage(w, "Signed in to ChatGPT. You can close this tab and return to Otto.")
		results <- callbackResult{code: code}
	})
	return mux
}

func writeBrowserMessage(w http.ResponseWriter, message string) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	_, _ = fmt.Fprintf(w, "<!doctype html><html><body><p>%s</p></body></html>", message)
}
