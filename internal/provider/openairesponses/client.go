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
	"github.com/baiyuqing/otto/internal/safetext"
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
	token, err := c.tokenSource.Token()
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return provider.Response{}, ctxErr
		}
		return provider.Response{}, errChatGPTAuthorizationFailed
	}
	if token == nil || strings.TrimSpace(token.AccessToken) == "" {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return provider.Response{}, ctxErr
		}
		return provider.Response{}, errChatGPTAuthorizationFailed
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
		return provider.Response{}, c.redactErr(token.AccessToken, c.accountID, fmt.Errorf("send responses request: %w", err), errChatGPTRequestFailed)
	}
	defer response.Body.Close()

	if response.StatusCode < 200 || response.StatusCode >= 300 {
		body, _ := io.ReadAll(io.LimitReader(response.Body, maxErrorBody))
		return provider.Response{}, c.redactErr(token.AccessToken, c.accountID,
			fmt.Errorf("chatgpt responses HTTP %d: %s", response.StatusCode, strings.TrimSpace(string(body))), errChatGPTRequestFailed)
	}

	result, _, err := readStream(response.Body, emit)
	if err != nil {
		return provider.Response{}, c.redactErr(token.AccessToken, c.accountID, err, errChatGPTRequestFailed)
	}
	return result, nil
}

// redactErr removes the rotated access token and account ID from error text so
// neither reaches logs, sessions, or the user. If exact shared redaction is
// impossible, it fails closed to a fixed fallback error.
func (c *Client) redactErr(token, accountID string, err, fallback error) error {
	if err == nil {
		return nil
	}
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	collector := safetext.NewSecretCollector()
	for _, value := range []string{token, accountID} {
		if value != "" && !collector.Add(value) {
			return fallback
		}
	}
	secrets := collector.Values()
	marker, ok := safetext.DynamicRedactionMarker(secrets)
	if !ok {
		if len(secrets) == 0 {
			return err
		}
		return fallback
	}
	message := err.Error()
	redacted := false
	for _, secret := range secrets {
		next := strings.ReplaceAll(message, secret, marker)
		if next != message {
			message = next
			redacted = true
		}
	}
	if !redacted {
		return err
	}
	return errors.New(message)
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
