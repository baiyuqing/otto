package config

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func write(t *testing.T, body string) string {
	t.Helper()
	p := filepath.Join(t.TempDir(), "connect.toml")
	if err := os.WriteFile(p, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	return p
}

const valid = `
[agent]
workspace = "/work/../work/proj/"

[telegram]
token_env = "TEST_TG_TOKEN"
chats = ["1"]
senders = ["2", "3"]
`

func TestLoadValidDefaults(t *testing.T) {
	t.Setenv("TEST_TG_TOKEN", "secret-value")
	cfg, err := Load(write(t, valid))
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(cfg.Agent.Command, []string{"otto", "acp"}) {
		t.Errorf("command = %v", cfg.Agent.Command)
	}
	if cfg.Agent.Workspace != "/work/proj" {
		t.Errorf("workspace = %q", cfg.Agent.Workspace)
	}
	tg := cfg.Telegram
	if tg == nil || tg.Token != "secret-value" || tg.TokenEnv != "TEST_TG_TOKEN" ||
		!reflect.DeepEqual(tg.Chats, []string{"1"}) || !reflect.DeepEqual(tg.Senders, []string{"2", "3"}) {
		t.Errorf("telegram = %+v", tg)
	}
}

func TestLoadCustomCommand(t *testing.T) {
	t.Setenv("TEST_TG_TOKEN", "x")
	cfg, err := Load(write(t, `[agent]
command = ["my-agent", "--flag"]
workspace = "/w"
[telegram]
token_env = "TEST_TG_TOKEN"
`))
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(cfg.Agent.Command, []string{"my-agent", "--flag"}) {
		t.Errorf("command = %v", cfg.Agent.Command)
	}
	if len(cfg.Telegram.Chats) != 0 || len(cfg.Telegram.Senders) != 0 {
		t.Errorf("empty lists expected: %+v", cfg.Telegram)
	}
}

func TestLoadErrors(t *testing.T) {
	t.Setenv("TEST_TG_TOKEN", "secret-value")
	t.Setenv("TEST_EMPTY", "")
	cases := []struct{ name, body, want string }{
		{"unknown key", valid + "bogus = 1\n", "bogus"},
		{"token in file", strings.Replace(valid, `token_env = "TEST_TG_TOKEN"`, "token_env = \"TEST_TG_TOKEN\"\ntoken = \"hunter2-literal\"", 1), "telegram.token"},
		{"env unset", strings.Replace(valid, "TEST_TG_TOKEN", "TEST_UNSET_VAR", 1), "TEST_UNSET_VAR"},
		{"env empty", strings.Replace(valid, "TEST_TG_TOKEN", "TEST_EMPTY", 1), "TEST_EMPTY"},
		{"relative workspace", strings.Replace(valid, "/work/../work/proj/", "rel/dir", 1), "absolute"},
		{"missing workspace", "[agent]\n[telegram]\ntoken_env = \"TEST_TG_TOKEN\"\n", "workspace"},
		{"empty command", "[agent]\ncommand = []\nworkspace = \"/w\"\n[telegram]\ntoken_env = \"TEST_TG_TOKEN\"\n", "command"},
		{"empty command element", "[agent]\ncommand = [\"\"]\nworkspace = \"/w\"\n[telegram]\ntoken_env = \"TEST_TG_TOKEN\"\n", "command"},
		{"missing token_env", "[agent]\nworkspace = \"/w\"\n[telegram]\n", "token_env"},
		{"no platform", "[agent]\nworkspace = \"/w\"\n", "[telegram]"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			_, err := Load(write(t, c.body))
			if err == nil {
				t.Fatal("expected error")
			}
			if !strings.Contains(err.Error(), c.want) {
				t.Errorf("error %q does not mention %q", err, c.want)
			}
			for _, secret := range []string{"hunter2-literal", "secret-value"} {
				if strings.Contains(err.Error(), secret) {
					t.Errorf("error leaks secret value: %q", err)
				}
			}
		})
	}
}

func TestLoadRefusesDirectACPKeys(t *testing.T) {
	t.Setenv("TEST_TG_TOKEN", "test-bot-token")
	for _, tc := range []struct{ name, line, key string }{
		{"address", "address = \"127.0.0.1:9001\"", "address"},
		{"token_env", "token_env = \"X\"", "token_env"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			body := strings.Replace(valid, "[agent]", "[agent]\n"+tc.line, 1)
			_, err := Load(write(t, body))
			if err == nil {
				t.Fatal("expected config rejection")
			}
			for _, want := range []string{tc.key, `acp", "--attach`} {
				if !strings.Contains(err.Error(), want) {
					t.Fatalf("error %q does not contain %q", err, want)
				}
			}
		})
	}
}

func TestMetricsListen(t *testing.T) {
	t.Setenv("TEST_TG_TOKEN", "secret-value")
	for _, tc := range []struct {
		name, table, want string
		wantErr           bool
	}{
		{"default", "", DefaultMetricsListen, false},
		{"empty turns it off", "[metrics]\nlisten = \"\"\n", "", false},
		{"ipv6 loopback", "[metrics]\nlisten = \"[::1]:9000\"\n", "[::1]:9000", false},
		{"localhost", "[metrics]\nlisten = \"localhost:9000\"\n", "localhost:9000", false},
		{"all interfaces", "[metrics]\nlisten = \"0.0.0.0:9465\"\n", "", true},
		{"no host", "[metrics]\nlisten = \":9465\"\n", "", true},
		{"public address", "[metrics]\nlisten = \"10.0.0.5:9465\"\n", "", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			cfg, err := Load(write(t, valid+tc.table))
			if tc.wantErr {
				if err == nil || !strings.Contains(err.Error(), "[metrics].listen") {
					t.Fatalf("err = %v, want [metrics].listen error", err)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if cfg.Metrics.Listen != tc.want {
				t.Fatalf("listen = %q, want %q", cfg.Metrics.Listen, tc.want)
			}
		})
	}
}
