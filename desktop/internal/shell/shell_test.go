package shell

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"reflect"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

func TestMain(m *testing.M) {
	if os.Getenv("OTTO_DESKTOP_HELPER") == "1" {
		if os.Args[1] == "trust" {
			if os.Getenv("TRUST_FAIL") == "1" {
				fmt.Fprintln(os.Stderr, "fixture trust failure")
				os.Exit(1)
			}
			os.Exit(0)
		}
		if os.Getenv("IGNORE_TERM") == "1" {
			signal.Ignore(syscall.SIGTERM)
		}
		data, _ := json.Marshal(struct {
			Args []string
			Env  []string
		}{os.Args[1:], os.Environ()})
		fmt.Fprintln(os.Stderr, string(data))
		fmt.Println("otto serve: http://127.0.0.1:1234/?token=fixture")
		_, _ = io.Copy(io.Discard, os.Stdin)
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func deadline(t *testing.T, duration time.Duration) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), duration)
	t.Cleanup(cancel)
	return ctx
}

func TestParseEnvironment(t *testing.T) {
	env, err := ParseEnvironment([]byte("banner\nOTTO_ENV_BEGINPATH=/usr/bin\x00FOO=a=b\x00BAR=line1\nline2\x00MALFORMED\x00\x00"))
	if err != nil || !reflect.DeepEqual(env, []string{"PATH=/usr/bin", "FOO=a=b", "BAR=line1\nline2"}) {
		t.Fatalf("environment = %q, error = %v", env, err)
	}
	if _, err := ParseEnvironment([]byte("PATH=/usr/bin\x00")); err == nil {
		t.Fatal("accepted output with no marker")
	}
}

func TestCaptureShellEnvironmentDeadline(t *testing.T) {
	script := filepath.Join(t.TempDir(), "probe")
	// The sleeping rc-file child retains stdout; cancellation must kill it
	// too, rather than waiting 30 seconds for the pipe to reach EOF.
	if err := os.WriteFile(script, []byte("#!/bin/sh\nsleep 30\n"), 0700); err != nil {
		t.Fatal(err)
	}
	started := time.Now()
	if _, err := captureShellEnvironment(deadline(t, 100*time.Millisecond), script); err == nil {
		t.Fatal("expected deadline failure")
	}
	if time.Since(started) > 2*time.Second {
		t.Fatal("probe descendants held stdout beyond the deadline")
	}
}

func TestStateCompatibility(t *testing.T) {
	home := t.TempDir()
	state, log := Locations(home)
	if state != filepath.Join(home, "Library/Application Support/com.otto.desktop/state.json") ||
		log != filepath.Join(home, "Library/Logs/com.otto.desktop/serve.log") {
		t.Fatalf("changed existing paths: %s, %s", state, log)
	}
	if got := LoadState(state); got != (State{}) {
		t.Fatalf("missing state: %+v", got)
	}
	want := State{Workspace: "/Users/me/project"}
	if err := SaveState(state, want); err != nil {
		t.Fatal(err)
	}
	if got := LoadState(state); got != want {
		t.Fatalf("roundtrip = %+v", got)
	}
	for _, data := range []string{`{"workspace":null}`, `not json`, `{"workspace":123}`} {
		if err := os.WriteFile(state, []byte(data), 0600); err != nil {
			t.Fatal(err)
		}
		if got := LoadState(state); got != (State{}) {
			t.Fatalf("legacy/corrupt state %s: %+v", data, got)
		}
	}
}

func TestServeURLAndNavigationBoundary(t *testing.T) {
	serve, err := ParseServeURL("http://127.0.0.1:1234/?a=1&token=abc%2B123")
	if err != nil || serve.Query().Get("token") != "abc+123" {
		t.Fatalf("serve = %v, %v", serve, err)
	}
	for _, raw := range []string{
		"http://example.com:1234/?token=t", "https://127.0.0.1:1234/?token=t",
		"http://127.0.0.1/?token=t", "http://127.0.0.1:0/?token=t",
		"http://127.0.0.1:1234/", "http://user@127.0.0.1:1234/?token=t",
	} {
		if _, err := ParseServeURL(raw); err == nil {
			t.Fatalf("accepted untrusted announcement %s", raw)
		}
	}
	for raw, want := range map[string]bool{
		"http://127.0.0.1:1234/__otto_desktop/open-folder":      true,
		"http://127.0.0.1:9999/__otto_desktop/open-folder":      false,
		"http://example.com:1234/__otto_desktop/open-folder":    false,
		"https://127.0.0.1:1234/__otto_desktop/open-folder":     false,
		"http://127.0.0.1:1234/":                                false,
		"http://user@127.0.0.1:1234/__otto_desktop/open-folder": false,
	} {
		if got := IsOpenFolderRequest(raw, serve); got != want {
			t.Errorf("navigation %s = %v, want %v", raw, got, want)
		}
	}
}

func TestWaitForServeURL(t *testing.T) {
	lines := make(chan string, 3)
	lines <- "warning: startup noise"
	lines <- "otto serve: "
	lines <- "otto serve: http://127.0.0.1:1234/?token=t"
	close(lines)
	if _, err := WaitForServeURL(deadline(t, time.Second), lines); err != nil {
		t.Fatal(err)
	}
	if _, err := WaitForServeURL(deadline(t, time.Second), lines); err == nil {
		t.Fatal("EOF accepted")
	}
	if _, err := WaitForServeURL(deadline(t, 30*time.Millisecond), make(chan string)); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected deadline, got %v", err)
	}
}

func TestPostWorkspace(t *testing.T) {
	path := "/Users/me/\"quoted\"/中文"
	requests := make(chan error, 1)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]string
		err := json.NewDecoder(r.Body).Decode(&body)
		if err == nil && (r.Method != "POST" || r.URL.Path != "/v1/workspaces" || r.URL.RawQuery != "" ||
			r.Header.Get("Authorization") != "Bearer fixture+token" || r.Header.Get("Content-Type") != "application/json" ||
			len(body) != 1 || body["path"] != path) {
			err = fmt.Errorf("unexpected workspace request: %s %s %+v", r.Method, r.URL.Path, body)
		}
		requests <- err
		w.WriteHeader(http.StatusCreated)
	}))
	defer server.Close()
	serve, _ := ParseServeURL(server.URL + "/?token=fixture%2Btoken")
	// A local native call must work even with an unusable system proxy.
	t.Setenv("HTTP_PROXY", "http://127.0.0.1:1")
	t.Setenv("NO_PROXY", "")
	if err := PostWorkspace(deadline(t, time.Second), serve, path); err != nil {
		t.Fatal(err)
	}
	if err := <-requests; err != nil {
		t.Fatal(err)
	}
}

func TestPostWorkspaceRejectsRedirectAndFailure(t *testing.T) {
	var leaked atomic.Int32
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { leaked.Add(1) }))
	defer target.Close()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target.URL, http.StatusTemporaryRedirect)
	}))
	defer server.Close()
	serve, _ := ParseServeURL(server.URL + "/?token=fixture")
	if err := PostWorkspace(deadline(t, time.Second), serve, "/project"); err == nil || !strings.Contains(err.Error(), "HTTP 307") {
		t.Fatalf("redirect: %v", err)
	}
	if leaked.Load() != 0 {
		t.Fatal("followed redirect outside the server")
	}
	bad, _ := url.Parse("http://example.com:1234/?token=fixture")
	if err := PostWorkspace(deadline(t, time.Second), bad, "/project"); err == nil {
		t.Fatal("posted to a non-loopback server")
	}
}

func helperChild(t *testing.T, extra ...string) (*Child, string) {
	t.Helper()
	log := filepath.Join(t.TempDir(), "serve.log")
	env := append([]string{"OTTO_DESKTOP_HELPER=1"}, extra...)
	child, err := SpawnServe(os.Args[0], "/workspace with spaces", env, log)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { child.Shutdown(time.Second) })
	if _, err := WaitForServeURL(deadline(t, 5*time.Second), child.Lines); err != nil {
		t.Fatal(err)
	}
	return child, log
}

func TestSpawnFlagsEnvironmentAndStdinLifetime(t *testing.T) {
	t.Setenv("OTTO_DESKTOP_PARENT_ONLY", "must not be inherited")
	child, log := helperChild(t)
	data, err := os.ReadFile(log)
	if err != nil {
		t.Fatal(err)
	}
	var recorded struct{ Args, Env []string }
	if err := json.Unmarshal(data, &recorded); err != nil {
		t.Fatal(err)
	}
	want := []string{"serve", "--cwd", "/workspace with spaces", "--listen", "127.0.0.1:0", "--exit-on-stdin-close"}
	if !reflect.DeepEqual(recorded.Args, want) {
		t.Fatalf("args = %q", recorded.Args)
	}
	for _, entry := range recorded.Env {
		if strings.HasPrefix(entry, "OTTO_DESKTOP_PARENT_ONLY=") {
			t.Fatal("inherited parent environment")
		}
	}
	if err := child.CloseStdin(); err != nil {
		t.Fatal(err)
	}
	if err := child.Wait(deadline(t, 5*time.Second)); err != nil {
		t.Fatal(err)
	}
}

func TestTrustFailure(t *testing.T) {
	err := Trust(deadline(t, 5*time.Second), os.Args[0], "/workspace", []string{"OTTO_DESKTOP_HELPER=1", "TRUST_FAIL=1"})
	if err == nil || !strings.Contains(err.Error(), "fixture trust failure") {
		t.Fatalf("trust error = %v", err)
	}
}

func TestShutdownAndQuitDuringStartup(t *testing.T) {
	for _, ignore := range []string{"0", "1"} {
		t.Run("ignore-term-"+ignore, func(t *testing.T) {
			child, _ := helperChild(t, "IGNORE_TERM="+ignore)
			child.Shutdown(50 * time.Millisecond)
			if err := child.Wait(deadline(t, time.Second)); errors.Is(err, context.DeadlineExceeded) {
				t.Fatal("child was not reaped after shutdown")
			}
			child.Shutdown(time.Second)
		})
	}
	var controller Controller
	controller.Stop(time.Second)
	child, _ := helperChild(t)
	if err := controller.Install(child); !errors.Is(err, context.Canceled) {
		t.Fatalf("installed child after quit: %v", err)
	}
	if err := child.Wait(deadline(t, time.Second)); errors.Is(err, context.DeadlineExceeded) {
		t.Fatal("startup child survived quit")
	}
	var running Controller
	child, _ = helperChild(t)
	if err := running.Install(child); err != nil {
		t.Fatal(err)
	}
	running.Stop(time.Second)
	if err := child.Wait(deadline(t, time.Second)); errors.Is(err, context.DeadlineExceeded) {
		t.Fatal("running child survived quit")
	}
}
