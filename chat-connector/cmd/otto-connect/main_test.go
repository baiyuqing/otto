package main

import (
	"bytes"
	"context"
	"log/slog"
	"net"
	"reflect"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/bridge"
	"github.com/baiyuqing/otto/chat-connector/internal/config"
	"github.com/baiyuqing/otto/chat-connector/internal/metrics"
)

func TestWithoutVarsDropsTokenVariable(t *testing.T) {
	env := []string{"PATH=/bin", "TG_TOKEN=123:abc", "TG_TOKEN_X=keep", "HOME=/h"}
	got := withoutVars(env, "TG_TOKEN")
	want := []string{"PATH=/bin", "TG_TOKEN_X=keep", "HOME=/h"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestPlatformsForTelegram(t *testing.T) {
	cfg := &config.Config{
		Telegram: &config.Telegram{TokenEnv: "TG_TOKEN", Token: "t", Chats: []string{"1"}, Senders: []string{"2"}},
	}
	platforms, access, secretVars := platformsFor(cfg, nil)
	var names []string
	for _, p := range platforms {
		names = append(names, p.Name())
	}
	if !reflect.DeepEqual(names, []string{"telegram"}) {
		t.Errorf("platforms = %v", names)
	}
	wantAccess := map[string]bridge.Access{
		"telegram": {Chats: []string{"1"}, Senders: []string{"2"}},
	}
	if !reflect.DeepEqual(access, wantAccess) {
		t.Errorf("access = %+v", access)
	}
	env := []string{"PATH=/bin", "TG_TOKEN=t", "HOME=/h"}
	if got := withoutVars(env, secretVars...); !reflect.DeepEqual(got, []string{"PATH=/bin", "HOME=/h"}) {
		t.Errorf("agent env = %v", got)
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
