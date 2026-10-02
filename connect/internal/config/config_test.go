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

const (
	feishuHead  = "[feishu]\n"
	feishuValid = feishuHead + "app_id = \"cli_test\"\napp_secret_env = \"TEST_FS_SECRET\"\n"
)

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
	t.Setenv("TEST_FS_SECRET", "fs-secret-value")
	t.Setenv("TEST_EMPTY", "")
	cases := []struct{ name, body, want string }{
		{"unknown key", valid + "bogus = 1\n", "bogus"},
		{"feishu missing app_id", valid + feishuHead + "app_secret_env = \"TEST_FS_SECRET\"\n", "app_id"},
		{"feishu missing app_secret_env", valid + feishuHead + "app_id = \"cli_test\"\n", "app_secret_env"},
		{"feishu secret in file", valid + feishuValid + "app_secret = \"fs-literal-secret\"\n", "feishu.app_secret"},
		{"feishu env unset", valid + feishuHead + "app_id = \"cli_test\"\napp_secret_env = \"TEST_UNSET_VAR\"\n", "TEST_UNSET_VAR"},
		{"feishu env empty", valid + feishuHead + "app_id = \"cli_test\"\napp_secret_env = \"TEST_EMPTY\"\n", "TEST_EMPTY"},
		{"feishu bad domain", valid + feishuValid + "domain = \"example\"\n", "domain"},
		{"token in file", strings.Replace(valid, `token_env = "TEST_TG_TOKEN"`, "token_env = \"TEST_TG_TOKEN\"\ntoken = \"hunter2-literal\"", 1), "telegram.token"},
		{"env unset", strings.Replace(valid, "TEST_TG_TOKEN", "TEST_UNSET_VAR", 1), "TEST_UNSET_VAR"},
		{"env empty", strings.Replace(valid, "TEST_TG_TOKEN", "TEST_EMPTY", 1), "TEST_EMPTY"},
		{"relative workspace", strings.Replace(valid, "/work/../work/proj/", "rel/dir", 1), "absolute"},
		{"missing workspace", "[agent]\n[telegram]\ntoken_env = \"TEST_TG_TOKEN\"\n", "workspace"},
		{"empty command", "[agent]\ncommand = []\nworkspace = \"/w\"\n[telegram]\ntoken_env = \"TEST_TG_TOKEN\"\n", "command"},
		{"empty command element", "[agent]\ncommand = [\"\"]\nworkspace = \"/w\"\n[telegram]\ntoken_env = \"TEST_TG_TOKEN\"\n", "command"},
		{"missing token_env", "[agent]\nworkspace = \"/w\"\n[telegram]\n", "token_env"},
		{"no platform", "[agent]\nworkspace = \"/w\"\n", "[feishu]"},
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
			for _, secret := range []string{"hunter2-literal", "secret-value", "fs-literal-secret", "fs-secret-value"} {
				if strings.Contains(err.Error(), secret) {
					t.Errorf("error leaks secret value: %q", err)
				}
			}
		})
	}
}

func TestLoadFeishuOnly(t *testing.T) {
	t.Setenv("TEST_FS_SECRET", "fs-secret-value")
	cfg, err := Load(write(t, "[agent]\nworkspace = \"/w\"\n"+feishuValid+"chats = [\"oc_test\"]\nsenders = [\"ou_test\"]\n"))
	if err != nil {
		t.Fatal(err)
	}
	fs := cfg.Feishu
	if cfg.Telegram != nil || fs == nil || fs.AppID != "cli_test" || fs.AppSecret != "fs-secret-value" ||
		fs.AppSecretEnv != "TEST_FS_SECRET" || fs.Domain != "feishu" ||
		!reflect.DeepEqual(fs.Chats, []string{"oc_test"}) || !reflect.DeepEqual(fs.Senders, []string{"ou_test"}) {
		t.Errorf("feishu = %+v, telegram = %+v", fs, cfg.Telegram)
	}
}

func TestLoadBothPlatformsAndLarkDomain(t *testing.T) {
	t.Setenv("TEST_TG_TOKEN", "secret-value")
	t.Setenv("TEST_FS_SECRET", "fs-secret-value")
	cfg, err := Load(write(t, valid+feishuValid+"domain = \"lark\"\n"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Telegram == nil || cfg.Feishu == nil || cfg.Feishu.Domain != "lark" {
		t.Errorf("telegram = %+v, feishu = %+v", cfg.Telegram, cfg.Feishu)
	}
}
