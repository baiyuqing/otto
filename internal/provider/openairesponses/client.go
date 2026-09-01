package openairesponses

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/provider"
	"golang.org/x/oauth2"
)

const (
	// defaultBaseURL is the ChatGPT backend that serves subscription traffic.
	defaultBaseURL = "https://chatgpt.com/backend-api/codex"
	// betaHeader and originator mirror the Codex CLI. Verify against
	// openai/codex before relying on the live backend.
	betaHeader = "responses=experimental"
	originator = "codex_cli_rs"

	defaultDialTimeout           = 30 * time.Second
	defaultKeepAlive             = 30 * time.Second
	defaultTLSHandshakeTimeout   = 10 * time.Second
	defaultResponseHeaderTimeout = 60 * time.Second
	defaultMaxHeaderBytes        = 1 << 20
)

// Client speaks the ChatGPT backend Responses API using an OAuth access token
// from a ChatGPT subscription.
type Client struct {
	baseURL     string
	tokenSource oauth2.TokenSource
	accountID   string
	httpClient  *http.Client
}

type contextTokenSource interface {
	TokenContext(context.Context) (*oauth2.Token, error)
}

var (
	_ provider.Provider     = (*Client)(nil)
	_ provider.RequestSizer = (*Client)(nil)

	errChatGPTAuthorizationFailed = errors.New("chatgpt authorization failed; run 'otto login'")
	errChatGPTRequestFailed       = errors.New("chatgpt request failed")
)

// New builds a subscription provider authorized by tokenSource. accountID is
// the chatgpt_account_id sent with every request. A nil httpClient uses a
// hardened default.
func New(tokenSource oauth2.TokenSource, accountID string, httpClient *http.Client) *Client {
	return newWithBaseURL(defaultBaseURL, tokenSource, accountID, httpClient)
}

func newWithBaseURL(baseURL string, tokenSource oauth2.TokenSource, accountID string, httpClient *http.Client) *Client {
	if httpClient == nil {
		httpClient = defaultHTTPClient()
	}
	return &Client{
		baseURL:     strings.TrimSuffix(baseURL, "/"),
		tokenSource: tokenSource,
		accountID:   accountID,
		httpClient:  httpClient,
	}
}

func (c *Client) SerializedRequestSize(request provider.Request) (int, error) {
	payload, err := json.Marshal(translateRequest(request))
	if err != nil {
		return 0, fmt.Errorf("encode responses request: %w", err)
	}
	return len(payload), nil
}

// Complete sends one request to the Responses backend. It does not retry;
// ponytail: no retry on 429/5xx, add a backoff loop (see openaicompat) if
// subscription rate limits make transient failures common.
func (c *Client) Complete(ctx context.Context, request provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
	if c.tokenSource == nil {
		return provider.Response{}, errChatGPTAuthorizationFailed
	}
	token, err := tokenForContext(ctx, c.tokenSource)
	if err != nil {
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return provider.Response{}, err
		}
		if ctxErr := ctx.Err(); ctxErr != nil {
			return provider.Response{}, ctxErr
		}
		return provider.Response{}, errChatGPTAuthorizationFailed
	}
	if token == nil || strings.TrimSpace(token.AccessToken) == "" || strings.TrimSpace(c.accountID) == "" {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return provider.Response{}, ctxErr
		}
		return provider.Response{}, errChatGPTAuthorizationFailed
	}
	requestRedactor, ok := newRequestRedactor(token.AccessToken, c.accountID)
	if !ok {
		return provider.Response{}, errChatGPTRequestFailed
	}
	payload, err := json.Marshal(translateRequest(request))
	if err != nil {
		return provider.Response{}, fmt.Errorf("encode responses request: %w", err)
	}

	httpRequest, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+"/responses", bytes.NewReader(payload))
	if err != nil {
		return provider.Response{}, fmt.Errorf("create responses request: %w", err)
	}
	httpRequest.Header.Set("Authorization", "Bearer "+token.AccessToken)
	httpRequest.Header.Set("chatgpt-account-id", c.accountID)
	httpRequest.Header.Set("Content-Type", "application/json")
	httpRequest.Header.Set("Accept", "text/event-stream")
	httpRequest.Header.Set("OpenAI-Beta", betaHeader)
	httpRequest.Header.Set("originator", originator)

	response, err := c.httpClient.Do(httpRequest)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return provider.Response{}, ctxErr
		}
		return provider.Response{}, errChatGPTRequestFailed
	}
	defer response.Body.Close()

	if response.StatusCode < 200 || response.StatusCode >= 300 {
		return provider.Response{}, fmt.Errorf("chatgpt responses HTTP %d", response.StatusCode)
	}

	emitter := requestRedactor.wrapEmit(emit)
	result, _, failureKind, err := readStream(response.Body, emitter.Emit)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return provider.Response{}, ctxErr
		}
		if failureKind == streamFailureRead {
			return provider.Response{}, errChatGPTRequestFailed
		}
		return provider.Response{}, errors.New(requestRedactor.redactString(err.Error()))
	}
	emitter.Flush()
	return requestRedactor.redactResponse(result), nil
}

func tokenForContext(ctx context.Context, source oauth2.TokenSource) (*oauth2.Token, error) {
	if contextual, ok := source.(contextTokenSource); ok {
		return contextual.TokenContext(ctx)
	}
	return source.Token()
}

func defaultHTTPClient() *http.Client {
	return &http.Client{
		Transport: &http.Transport{
			Proxy:                  http.ProxyFromEnvironment,
			DialContext:            (&net.Dialer{Timeout: defaultDialTimeout, KeepAlive: defaultKeepAlive}).DialContext,
			TLSHandshakeTimeout:    defaultTLSHandshakeTimeout,
			ResponseHeaderTimeout:  defaultResponseHeaderTimeout,
			MaxResponseHeaderBytes: defaultMaxHeaderBytes,
			ForceAttemptHTTP2:      true,
		},
		CheckRedirect: refuseResponsesRedirects,
	}
}

func refuseResponsesRedirects(*http.Request, []*http.Request) error {
	return http.ErrUseLastResponse
}
