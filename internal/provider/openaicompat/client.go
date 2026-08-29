package openaicompat

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/provider"
)

const maxErrorBody = 32 << 10

const (
	defaultDialTimeout           = 30 * time.Second
	defaultKeepAlive             = 30 * time.Second
	defaultTLSHandshakeTimeout   = 10 * time.Second
	defaultResponseHeaderTimeout = 60 * time.Second
	defaultMaxHeaderBytes        = 1 << 20
	maxRedirects                 = 3
)

type Client struct {
	baseURL    string
	apiKey     string
	httpClient *http.Client
	err        error
	sleep      func(context.Context, time.Duration) error
}

var (
	_ provider.Provider     = (*Client)(nil)
	_ provider.RequestSizer = (*Client)(nil)
)

func New(baseURL, apiKey string, httpClient *http.Client) *Client {
	client := &Client{
		apiKey:     apiKey,
		httpClient: httpClient,
		sleep:      sleepContext,
	}
	if client.httpClient == nil {
		client.httpClient = defaultHTTPClient()
	}
	client.baseURL, client.err = NormalizeBaseURL(baseURL)
	return client
}

func NormalizeBaseURL(baseURL string) (string, error) {
	parsed, err := url.Parse(baseURL)
	if err != nil || (parsed.Scheme != "http" && parsed.Scheme != "https") || parsed.Host == "" || parsed.User != nil || parsed.ForceQuery || parsed.RawQuery != "" || parsed.Fragment != "" {
		return "", errors.New("invalid OpenAI-compatible base URL")
	}
	parsed.Path = strings.TrimSuffix(parsed.Path, "/")
	parsed.RawPath = strings.TrimSuffix(parsed.RawPath, "/")
	return parsed.String(), nil
}

func (c *Client) SerializedRequestSize(request provider.Request) (int, error) {
	payload, err := json.Marshal(translateRequest(request))
	if err != nil {
		return 0, fmt.Errorf("encode chat completion request: %w", err)
	}
	return len(payload), nil
}

func (c *Client) Complete(ctx context.Context, request provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
	if c.err != nil {
		return provider.Response{}, c.err
	}
	payload, err := json.Marshal(translateRequest(request))
	if err != nil {
		return provider.Response{}, c.safeError(fmt.Errorf("encode chat completion request: %w", err))
	}

	for attempt := 0; attempt < 3; attempt++ {
		result, emitted, retryable, retryDelay, err := c.attempt(ctx, payload, emit)
		if err == nil {
			return result, nil
		}
		if ctx.Err() != nil {
			return provider.Response{}, ctx.Err()
		}
		if !retryable || emitted || attempt == 2 {
			return provider.Response{}, c.safeError(err)
		}
		if retryDelay == nil {
			delay := 250 * time.Millisecond * time.Duration(1<<attempt)
			retryDelay = &delay
		}
		if err := c.sleep(ctx, *retryDelay); err != nil {
			if ctxErr := ctx.Err(); ctxErr != nil {
				return provider.Response{}, ctxErr
			}
			return provider.Response{}, c.safeError(err)
		}
	}
	panic("unreachable")
}

func (c *Client) attempt(ctx context.Context, payload []byte, emit func(provider.StreamEvent)) (provider.Response, bool, bool, *time.Duration, error) {
	httpRequest, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+"/chat/completions", bytes.NewReader(payload))
	if err != nil {
		return provider.Response{}, false, false, nil, fmt.Errorf("create chat completion request: %w", err)
	}
	httpRequest.Header.Set("Authorization", "Bearer "+c.apiKey)
	httpRequest.Header.Set("Content-Type", "application/json")
	httpRequest.Header.Set("Accept", "text/event-stream")
	response, err := c.httpClient.Do(httpRequest)
	if err != nil {
		retryable := response == nil && ctx.Err() == nil
		return provider.Response{}, false, retryable, nil, fmt.Errorf("send chat completion request: %w", err)
	}
	defer response.Body.Close()

	if response.StatusCode < 200 || response.StatusCode >= 300 {
		delay := parseRetryAfter(response.Header.Get("Retry-After"))
		body, readErr := io.ReadAll(io.LimitReader(response.Body, maxErrorBody+1))
		if readErr != nil {
			return provider.Response{}, false, isRetryableStatus(response.StatusCode), delay, fmt.Errorf("OpenAI-compatible HTTP %d (error body unreadable)", response.StatusCode)
		}
		body = body[:min(len(body), maxErrorBody)]
		safeBody := c.redactErrorBody(body)
		if overflow := classifyContextOverflow(response.StatusCode, safeBody); overflow != nil {
			return provider.Response{}, false, false, nil, overflow
		}
		err := fmt.Errorf("OpenAI-compatible HTTP %d: %s", response.StatusCode, strings.TrimSpace(string(safeBody)))
		return provider.Response{}, false, isRetryableStatus(response.StatusCode), delay, err
	}

	result, emitted, err := readStream(response.Body, emit)
	if err == nil {
		return result, emitted, false, nil, nil
	}
	var readErr *streamReadError
	return provider.Response{}, emitted, errors.As(err, &readErr), nil, err
}

func isRetryableStatus(status int) bool {
	return status == http.StatusTooManyRequests || status >= 500 && status <= 599
}

func parseRetryAfter(value string) *time.Duration {
	if value == "" {
		return nil
	}
	if seconds, err := strconv.ParseInt(strings.TrimSpace(value), 10, 64); err == nil && seconds >= 0 {
		delay := time.Duration(seconds) * time.Second
		return &delay
	}
	if retryAt, err := http.ParseTime(value); err == nil {
		delay := time.Until(retryAt)
		if delay < 0 {
			delay = 0
		}
		return &delay
	}
	return nil
}

func (c *Client) safeError(err error) error {
	if err == nil {
		return nil
	}
	var overflow *provider.ContextOverflowError
	if errors.As(err, &overflow) && overflow != nil {
		return overflow
	}
	if c.apiKey == "" {
		return err
	}
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	return errors.New(string(c.redactErrorBody([]byte(err.Error()))))
}

func (c *Client) redactErrorBody(body []byte) []byte {
	if c.apiKey == "" {
		return body
	}
	replacement := "[REDACTED]"
	if len(replacement) > len(c.apiKey) {
		replacement = strings.Repeat("*", len(c.apiKey))
	}
	return []byte(strings.ReplaceAll(string(body), c.apiKey, replacement))
}

func sleepContext(ctx context.Context, delay time.Duration) error {
	timer := time.NewTimer(delay)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

// defaultHTTPClient returns the hardened client used when callers do not
// supply their own. The zero http.Client (http.DefaultClient) has no dial,
// handshake, response-header, or redirect limits, so a stalled or malicious
// endpoint could hold a request open indefinitely. Streaming chat completion
// bodies can legitimately stay open for minutes, so there is intentionally no
// overall Client.Timeout; cancellation still works through the request context
// and the transport-level timeouts bound every pre-body phase.
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
		CheckRedirect: redirectPolicy,
	}
}

// redirectPolicy follows only same-origin (scheme and host) redirects and
// stops after maxRedirects hops. Go already strips the Authorization header on
// cross-host redirects; blocking them outright removes the remaining leak
// surface instead of relying on that default.
func redirectPolicy(request *http.Request, via []*http.Request) error {
	if len(via) >= maxRedirects {
		return errors.New("stopped after 3 redirects")
	}
	if len(via) > 0 {
		previous := via[len(via)-1]
		if request.URL.Host != previous.URL.Host || request.URL.Scheme != previous.URL.Scheme {
			return errors.New("redirect to a different origin is blocked")
		}
	}
	return nil
}
