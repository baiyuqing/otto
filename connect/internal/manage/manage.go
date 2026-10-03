// Package manage is the restricted Unix-socket client for otto serve's
// configuration-management API. It is used only when connect starts
// `otto acp --attach`; it never reads or writes config.toml itself.
package manage

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"path/filepath"
	"strings"
	"time"
)

const defaultSocketSuffix = ".otto/otto.sock"

type requestIDKey struct{}

// WithRequestID attaches a non-authoritative correlation ID to one management
// request. It is for diagnostics only, never authorization.
func WithRequestID(ctx context.Context, id string) context.Context {
	return context.WithValue(ctx, requestIDKey{}, id)
}

type Client struct {
	http *http.Client
	base string
}

// NewWithBaseURL wraps a caller-owned HTTP client and base URL. Production uses
// FromCommand; tests use this constructor with httptest so they need not bind a
// Unix socket.
func NewWithBaseURL(httpClient *http.Client, baseURL string) *Client {
	return &Client{http: httpClient, base: strings.TrimRight(baseURL, "/")}
}

type APIError struct{ Code, Message string }

func (e *APIError) Error() string { return e.Code + ": " + e.Message }

type Profile struct {
	Name      string `json:"name"`
	Default   bool   `json:"default"`
	Provider  string `json:"provider"`
	Model     string `json:"model"`
	Thinking  string `json:"thinking"`
	BaseURL   string `json:"base_url"`
	APIKeyEnv string `json:"api_key_env"`
}
type Preview struct {
	ID, Operation, Profile, Field, Diff string
	ExpiresInSeconds                    uint64 `json:"expires_in_seconds"`
}

// FromCommand recognizes the existing attach command and its optional socket.
// home selects the same default socket path Otto uses when --socket is absent.
func FromCommand(command []string, home string) (*Client, error) {
	attach := false
	socket := ""
	for i, arg := range command {
		if arg == "--attach" {
			attach = true
		}
		if arg == "--socket" && i+1 < len(command) {
			socket = command[i+1]
		}
	}
	if !attach {
		return nil, errors.New("management requires otto acp --attach connected to otto serve")
	}
	if socket == "" {
		socket = filepath.Join(home, defaultSocketSuffix)
	}
	transport := &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
		return (&net.Dialer{}).DialContext(ctx, "unix", socket)
	}}
	return &Client{http: &http.Client{Transport: transport, Timeout: 30 * time.Second}, base: "http://otto"}, nil
}

func (c *Client) Profiles(ctx context.Context) ([]Profile, error) {
	var out struct {
		Profiles []Profile `json:"profiles"`
	}
	if err := c.get(ctx, "/v1/config/profiles", &out); err != nil {
		return nil, err
	}
	return out.Profiles, nil
}
func (c *Client) Profile(ctx context.Context, name string) (Profile, error) {
	var out Profile
	if err := c.get(ctx, "/v1/config/profiles/"+url.PathEscape(name), &out); err != nil {
		return Profile{}, err
	}
	return out, nil
}
func (c *Client) Models(ctx context.Context, profile string) ([]string, error) {
	var out struct {
		Models []string `json:"models"`
	}
	if err := c.get(ctx, "/v1/config/models?profile="+url.QueryEscape(profile), &out); err != nil {
		return nil, err
	}
	return out.Models, nil
}
func (c *Client) Preview(ctx context.Context, change any) (Preview, error) {
	var out Preview
	if err := c.request(ctx, http.MethodPost, "/v1/config/changes", change, &out); err != nil {
		return Preview{}, err
	}
	return out, nil
}
func (c *Client) Confirm(ctx context.Context, id string) error {
	return c.request(ctx, http.MethodPost, "/v1/config/changes/"+url.PathEscape(id), nil, nil)
}
func (c *Client) Cancel(ctx context.Context, id string) error {
	return c.request(ctx, http.MethodDelete, "/v1/config/changes/"+url.PathEscape(id), nil, nil)
}
func (c *Client) get(ctx context.Context, path string, out any) error {
	return c.request(ctx, http.MethodGet, path, nil, out)
}
func (c *Client) request(ctx context.Context, method, path string, body, out any) error {
	var reader *strings.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return err
		}
		reader = strings.NewReader(string(raw))
	} else {
		reader = strings.NewReader("")
	}
	req, err := http.NewRequestWithContext(ctx, method, c.base+path, reader)
	if err != nil {
		return err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if id, ok := ctx.Value(requestIDKey{}).(string); ok && id != "" {
		req.Header.Set("X-Otto-Request-Id", id)
	}
	res, err := c.http.Do(req)
	if err != nil {
		return fmt.Errorf("otto serve is unreachable: %w", err)
	}
	defer res.Body.Close()
	if res.StatusCode < 200 || res.StatusCode >= 300 {
		var reply struct {
			Error APIError `json:"error"`
		}
		_ = json.NewDecoder(res.Body).Decode(&reply)
		return &reply.Error
	}
	if out != nil {
		return json.NewDecoder(res.Body).Decode(out)
	}
	return nil
}
