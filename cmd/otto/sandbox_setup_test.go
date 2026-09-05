package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/sandbox"
)

func TestSandboxSetup(t *testing.T) {
	for _, tc := range []struct {
		name, input string
		saved       bool
	}{
		{"save", "n\ny\nsave\n", true}, {"cancel", "y\ny\ncancel\n", false}, {"eof", "y\ny\n", false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			home := t.TempDir()
			gh := filepath.Join(home, ".config", "gh")
			if err := os.MkdirAll(gh, 0700); err != nil {
				t.Fatal(err)
			}
			path := filepath.Join(home, "config.toml")
			original := "# preserved\n[profiles.demo]\nmodel = 'example'\n"
			if err := os.WriteFile(path, []byte(original), 0600); err != nil {
				t.Fatal(err)
			}
			var out, errs bytes.Buffer
			code := run(context.Background(), []string{"sandbox", "setup", "--config", path}, strings.NewReader(tc.input), &out, &errs, func() []string { return []string{"HOME=" + home} })
			if code != 0 {
				t.Fatalf("code %d: %s", code, &errs)
			}
			data, _ := os.ReadFile(path)
			if !tc.saved {
				if string(data) != original {
					t.Fatal("changed on cancel")
				}
				return
			}
			cfg, err := config.LoadRequired(path)
			if err != nil {
				t.Fatal(err)
			}
			if cfg.Sandbox.Network == nil || *cfg.Sandbox.Network != "deny" || len(cfg.Sandbox.ReadPaths) != 1 || cfg.Sandbox.ReadPaths[0] != func() string { p, _ := filepath.EvalSymlinks(gh); return p }() {
				t.Fatalf("%+v", cfg.Sandbox)
			}
			if !strings.Contains(out.String(), "GH_CONFIG_DIR=") || !strings.Contains(string(data), original) {
				t.Fatalf("missing launch instructions or preserved content: %s", &out)
			}
		})
	}
}

func TestSandboxSetupCheck(t *testing.T) {
	for _, tc := range []struct {
		name      string
		available bool
		code      int
		message   string
	}{
		{"startup", false, 0, "Sandbox startup failed"},
		{"success", true, 0, "authentication and network connectivity were not tested"},
		{"missing gh", true, 20, "not on the sandbox PATH"},
		{"unreadable", true, 21, "not readable inside the sandbox"},
		{"other failure", true, 22, "does not by itself establish"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			closed := false
			var out bytes.Buffer
			checkSandboxSetup(context.Background(), func(context.Context, sandboxOpenOptions) sandboxRuntime {
				return sandboxRuntime{Info: app.SandboxInfo{BashAvailable: tc.available}, Executor: setupExecutor{code: tc.code}, close: func() error { closed = true; return nil }}
			}, sandboxOpenOptions{}, true, &out)
			if !closed || !strings.Contains(out.String(), tc.message) {
				t.Fatalf("closed %v: %s", closed, &out)
			}
		})
	}
}

type setupExecutor struct{ code int }

func (e setupExecutor) Execute(_ context.Context, r sandbox.Request, _ sandbox.Streams) (sandbox.ExitStatus, error) {
	return sandbox.ExitStatus{Code: e.code}, nil
}

func TestSandboxSetupSaveGuards(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.toml")
	if err := os.WriteFile(path, []byte("changed"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := saveSandboxSetup(path, []byte("old"), []byte("new")); err == nil {
		t.Fatal("overwrote concurrent edit")
	}
	link := filepath.Join(dir, "link")
	if err := os.Symlink(path, link); err != nil {
		t.Fatal(err)
	}
	if err := saveSandboxSetup(link, []byte("changed"), []byte("new")); err == nil {
		t.Fatal("accepted symlink")
	}
	data, _ := os.ReadFile(path)
	if string(data) != "changed" {
		t.Fatal("changed original")
	}
}
