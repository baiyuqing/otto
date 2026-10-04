package main

import (
	"bytes"
	"context"
	"log/slog"
	"net"
	"reflect"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/connect/internal/bridge"
	"github.com/baiyuqing/otto/connect/internal/config"
	"github.com/baiyuqing/otto/connect/internal/metrics"
)

func TestWithoutVarsDropsTokenVariable(t *testing.T) {
	env := []string{"PATH=/bin", "TG_TOKEN=123:abc", "TG_TOKEN_X=keep", "HOME=/h"}
	got := withoutVars(env, "TG_TOKEN")
	want := []string{"PATH=/bin", "TG_TOKEN_X=keep", "HOME=/h"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestPlatformsForBothPlatforms(t *testing.T) {
	cfg := &config.Config{
		Telegram: &config.Telegram{TokenEnv: "TG_TOKEN", Token: "t", Chats: []string{"1"}, Senders: []string{"2"}},
		Feishu: &config.Feishu{AppID: "cli_test", AppSecretEnv: "FS_SECRET", AppSecret: "s", Domain: "feishu",
			Chats: []string{"oc_test"}, Senders: []string{"ou_test"}},
	}
	platforms, access, secretVars, err := platformsFor(cfg, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, p := range platforms {
		names = append(names, p.Name())
	}
	if !reflect.DeepEqual(names, []string{"telegram", "feishu"}) {
		t.Errorf("platforms = %v", names)
	}
	wantAccess := map[string]bridge.Access{
		"telegram": {Chats: []string{"1"}, Senders: []string{"2"}},
		"feishu":   {Chats: []string{"oc_test"}, Senders: []string{"ou_test"}},
	}
	if !reflect.DeepEqual(access, wantAccess) {
		t.Errorf("access = %+v", access)
	}
	env := []string{"PATH=/bin", "TG_TOKEN=t", "FS_SECRET=s", "HOME=/h"}
	if got := withoutVars(env, secretVars...); !reflect.DeepEqual(got, []string{"PATH=/bin", "HOME=/h"}) {
		t.Errorf("agent env = %v", got)
	}
}

func TestPlatformsForFeishuOnly(t *testing.T) {
	cfg := &config.Config{Feishu: &config.Feishu{AppID: "cli_test", AppSecretEnv: "FS_SECRET", AppSecret: "s", Domain: "lark"}}
	platforms, access, secretVars, err := platformsFor(cfg, nil, nil)
	if err != nil || len(platforms) != 1 || platforms[0].Name() != "feishu" ||
		!reflect.DeepEqual(secretVars, []string{"FS_SECRET"}) || len(access["feishu"].Chats) != 0 {
		t.Errorf("platforms = %v, access = %v, vars = %v, err = %v", platforms, access, secretVars, err)
	}
}

func TestPlatformsForWithholdsACPToken(t *testing.T) {
	cfg := &config.Config{Agent: config.Agent{TokenEnv: "ACP_TOKEN"}, Telegram: &config.Telegram{TokenEnv: "TG_TOKEN"}}
	_, _, names, err := platformsFor(cfg, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	got := withoutVars([]string{"PATH=/bin", "ACP_TOKEN=test-transport-token", "TG_TOKEN=test-bot-token"}, names...)
	if !reflect.DeepEqual(got, []string{"PATH=/bin"}) {
		t.Fatal("transport credentials reached agent environment")
	}
}

func TestServeMetricsBusyPortLogsAndReturns(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	var logs bytes.Buffer
	old := slog.Default()
	slog.SetDefault(slog.New(slog.NewTextHandler(&logs, nil)))
	defer slog.SetDefault(old)
	serveMetrics(context.Background(), metrics.New(), ln.Addr().String())
	for _, want := range []string{"level=WARN", "msg=metrics_listen_failed", "address=" + ln.Addr().String(), "error="} {
		if !strings.Contains(logs.String(), want) {
			t.Errorf("log %q lacks %q", logs.String(), want)
		}
	}
}
