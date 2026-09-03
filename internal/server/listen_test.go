package server

import (
	"context"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
	"testing"
)

func TestListenCreatesParentDirectoryWithPrivateMode(t *testing.T) {
	t.Chdir(t.TempDir())
	l, err := Listen("sub/otto.sock")
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	defer l.Close()

	info, err := os.Stat("sub")
	if err != nil {
		t.Fatalf("stat parent dir: %v", err)
	}
	if !info.IsDir() {
		t.Fatalf("expected sub to be a directory")
	}
	if perm := info.Mode().Perm(); perm != 0o700 {
		t.Errorf("parent dir mode = %o, want 0700", perm)
	}
}

func TestListenSocketFileMode(t *testing.T) {
	t.Chdir(t.TempDir())
	l, err := Listen("sub/otto.sock")
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	defer l.Close()

	info, err := os.Stat("sub/otto.sock")
	if err != nil {
		t.Fatalf("stat socket: %v", err)
	}
	if perm := info.Mode().Perm(); perm != 0o600 {
		t.Errorf("socket mode = %o, want 0600", perm)
	}
}

func TestListenTwiceReturnsAlreadyRunning(t *testing.T) {
	t.Chdir(t.TempDir())
	l1, err := Listen("sub/otto.sock")
	if err != nil {
		t.Fatalf("first Listen: %v", err)
	}
	defer l1.Close()

	_, err = Listen("sub/otto.sock")
	if err == nil {
		t.Fatal("expected error on second Listen")
	}
	if !strings.Contains(err.Error(), "already running") || !strings.Contains(err.Error(), "sub/otto.sock") {
		t.Errorf("error = %q, want it to contain %q and the path", err.Error(), "already running")
	}
}

func TestListenRemovesStaleSocket(t *testing.T) {
	t.Chdir(t.TempDir())
	l1, err := Listen("sub/otto.sock")
	if err != nil {
		t.Fatalf("first Listen: %v", err)
	}
	unixListener := l1.(*net.UnixListener)
	unixListener.SetUnlinkOnClose(false)
	if err := l1.Close(); err != nil {
		t.Fatalf("close first listener: %v", err)
	}

	l2, err := Listen("sub/otto.sock")
	if err != nil {
		t.Fatalf("Listen should remove the stale socket and succeed: %v", err)
	}
	defer l2.Close()
}

func TestListenPreservesNonSocketFile(t *testing.T) {
	t.Chdir(t.TempDir())
	if err := os.Mkdir("sub", 0o700); err != nil {
		t.Fatal(err)
	}
	const path = "sub/otto.sock"
	if err := os.WriteFile(path, []byte("keep me"), 0o600); err != nil {
		t.Fatal(err)
	}

	if _, err := Listen(path); err == nil {
		t.Fatal("Listen should reject an existing non-socket file")
	}
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("existing file was removed: %v", err)
	}
	if string(content) != "keep me" {
		t.Fatalf("existing file content = %q", content)
	}
}

func TestListenRejectsUnsafeParentDirectory(t *testing.T) {
	t.Chdir(t.TempDir())
	if err := os.MkdirAll("sub", 0o777); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	if err := os.Chmod("sub", 0o777); err != nil {
		t.Fatalf("chmod: %v", err)
	}

	_, err := Listen("sub/otto.sock")
	if err == nil {
		t.Fatal("expected error for a group/world-writable parent directory")
	}
}

func TestListenHTTPRoundTrip(t *testing.T) {
	t.Chdir(t.TempDir())
	l, err := Listen("sub/otto.sock")
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	defer l.Close()

	mux := http.NewServeMux()
	mux.HandleFunc("/ping", func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte("pong"))
	})
	go func() { _ = http.Serve(l, mux) }()

	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", "sub/otto.sock")
			},
		},
	}
	resp, err := client.Get("http://unix/ping")
	if err != nil {
		t.Fatalf("Get: %v", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("ReadAll: %v", err)
	}
	if string(body) != "pong" {
		t.Errorf("body = %q, want %q", body, "pong")
	}
}
