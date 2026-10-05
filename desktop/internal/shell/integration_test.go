package shell

import (
	"context"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

// OTTO_BIN is supplied by desktop-check, so these exercise the same real
// Rust process and flags as the app without requiring a native webview.
func TestRealServeLifecycle(t *testing.T) {
	binary := os.Getenv("OTTO_BIN")
	if binary == "" {
		t.Skip("set OTTO_BIN or run make desktop-check for real server lifecycle coverage")
	}
	if !filepath.IsAbs(binary) {
		t.Fatal("OTTO_BIN must be absolute")
	}
	for _, closeStdin := range []bool{true, false} {
		name := "SIGTERM"
		if closeStdin {
			name = "stdin-close"
		}
		t.Run(name, func(t *testing.T) {
			home, workspace := t.TempDir(), t.TempDir()
			configDir := filepath.Join(home, ".config", "otto")
			if err := os.MkdirAll(configDir, 0700); err != nil {
				t.Fatal(err)
			}
			config := "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"http://127.0.0.1:1\"\nmodel = \"test-model\"\napi_key_env = \"OTTO_API_KEY\"\n"
			if err := os.WriteFile(filepath.Join(configDir, "config.toml"), []byte(config), 0600); err != nil {
				t.Fatal(err)
			}
			env := []string{"HOME=" + home, "OTTO_API_KEY=sk-test-fixture"}
			if err := Trust(deadline(t, 10*time.Second), binary, workspace, env); err != nil {
				t.Fatal(err)
			}
			child, err := SpawnServe(binary, workspace, env, filepath.Join(home, "serve.log"))
			if err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { child.Shutdown(time.Second) })
			serve, err := WaitForServeURL(deadline(t, 10*time.Second), child.Lines)
			if err != nil {
				t.Fatal(DescribeStartupFailure(child, err, filepath.Join(home, "serve.log")))
			}
			client := &http.Client{Timeout: time.Second, Transport: &http.Transport{}}
			res, err := client.Get("http://" + serve.Host + "/healthz")
			if err != nil {
				t.Fatal(err)
			}
			res.Body.Close()
			client.CloseIdleConnections()
			if res.StatusCode != http.StatusOK {
				t.Fatalf("healthz = %d", res.StatusCode)
			}
			// Register another workspace through the shell's authenticated HTTP
			// client, then verify the real server accepts the request.
			another := t.TempDir()
			if err := Trust(deadline(t, 10*time.Second), binary, another, env); err != nil {
				t.Fatal(err)
			}
			if err := PostWorkspace(deadline(t, 5*time.Second), serve, another); err != nil {
				t.Fatal(err)
			}
			if closeStdin {
				if err := child.CloseStdin(); err != nil {
					t.Fatal(err)
				}
			} else {
				if err := child.cmd.Process.Signal(syscall.SIGTERM); err != nil {
					t.Fatal(err)
				}
			}
			if err := child.Wait(deadline(t, 5*time.Second)); errors.Is(err, context.DeadlineExceeded) {
				t.Fatal("otto serve did not exit within 5 seconds")
			} else if err != nil {
				t.Fatalf("otto serve exit: %v", err)
			}
		})
	}
}
