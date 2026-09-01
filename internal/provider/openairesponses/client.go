package openairesponses

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
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

	maxErrorBody = 32 << 10

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

var (
	_ provider.Provider     = (*Client)(nil)
	_ provider.RequestSizer = (*Client)(nil)
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
		return provider.Response{}, errors.New("no chatgpt credentials; run 'otto login'")
	}
	token, err := c.tokenSource.Token()
	if err != nil {
		return provider.Response{}, fmt.Errorf("authorize chatgpt request: %w", err)
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
		return provider.Response{}, c.redactErr(token.AccessToken, fmt.Errorf("send responses request: %w", err))
	}
	defer response.Body.Close()

	if response.StatusCode < 200 || response.StatusCode >= 300 {
		body, _ := io.ReadAll(io.LimitReader(response.Body, maxErrorBody))
		return provider.Response{}, c.redactErr(token.AccessToken,
			fmt.Errorf("chatgpt responses HTTP %d: %s", response.StatusCode, strings.TrimSpace(string(body))))
	}

	result, _, err := readStream(response.Body, emit)
	if err != nil {
		return provider.Response{}, c.redactErr(token.AccessToken, err)
	}
	return result, nil
}

// redactErr removes the access token from error text so it never reaches logs,
// sessions, or the user. Tokens rotate, so the runtime redactor (built from
// static secrets) cannot cover them; the provider redacts the value it used.
func (c *Client) redactErr(token string, err error) error {
	if err == nil || token == "" {
		return err
	}
	msg := strings.ReplaceAll(err.Error(), token, "[REDACTED]")
	if msg == err.Error() {
		return err
	}
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	return errors.New(msg)
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
	}
}
