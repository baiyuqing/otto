package shell

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

const OpenFolderPath = "/__otto_desktop/open-folder"

// No bound Go services or broad IPC permissions are exposed to the page.
const PreloadScript = `if (window === window.top) {
  window.__OTTO_DESKTOP__ = Object.freeze({
    openFolder: () => window.location.assign('/__otto_desktop/open-folder')
  });
}`

// ParseServeURL validates the child's announcement before using its token
// or making native HTTP requests. The shell only launches IPv4 loopback.
func ParseServeURL(raw string) (*url.URL, error) {
	u, err := url.Parse(raw)
	if err != nil {
		return nil, errors.New("invalid otto serve URL")
	}
	port, err := strconv.Atoi(u.Port())
	if err != nil || port < 1 || port > 65535 || u.Scheme != "http" ||
		u.Hostname() != "127.0.0.1" || u.User != nil || u.Fragment != "" || u.Path != "/" || u.Query().Get("token") == "" {
		return nil, errors.New("otto serve must announce an authenticated HTTP loopback URL")
	}
	return u, nil
}

func IsOpenFolderRequest(raw string, serve *url.URL) bool {
	u, err := url.Parse(raw)
	return err == nil && u.User == nil && u.Scheme == serve.Scheme &&
		strings.EqualFold(u.Host, serve.Host) && u.Path == OpenFolderPath
}

// Irrelevant lines do not reset the startup deadline. EOF is reported
// immediately instead of waiting for the timeout after an early exit.
func WaitForServeURL(ctx context.Context, lines <-chan string) (*url.URL, error) {
	for {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case line, ok := <-lines:
			if !ok {
				return nil, errors.New("otto serve closed stdout before announcing its URL")
			}
			if raw, ok := strings.CutPrefix(line, "otto serve: "); ok && strings.TrimSpace(raw) != "" {
				return ParseServeURL(strings.TrimSpace(raw))
			}
		}
	}
}

// Proxy-free, bounded loopback requests must never send the server token
// to a configured proxy or follow a redirect to another origin.
func PostWorkspace(ctx context.Context, serve *url.URL, path string) error {
	if _, err := ParseServeURL(serve.String()); err != nil {
		return err
	}
	body, err := json.Marshal(struct {
		Path string `json:"path"`
	}{path})
	if err != nil {
		return err
	}
	endpoint := &url.URL{Scheme: serve.Scheme, Host: serve.Host, Path: "/v1/workspaces"}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint.String(), bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+serve.Query().Get("token"))
	req.Header.Set("Content-Type", "application/json")
	transport := &http.Transport{DisableKeepAlives: true}
	defer transport.CloseIdleConnections()
	client := &http.Client{Transport: transport, Timeout: 10 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	res, err := client.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	if res.StatusCode < 200 || res.StatusCode >= 300 {
		body, _ := io.ReadAll(io.LimitReader(res.Body, 64<<10))
		return fmt.Errorf("registering the workspace failed: HTTP %d\n\n%s", res.StatusCode, body)
	}
	return nil
}
