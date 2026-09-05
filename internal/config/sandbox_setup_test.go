package config

import (
	"strings"
	"testing"
)

func TestUpdateSandbox(t *testing.T) {
	network := "deny"
	raw := SandboxConfig{Network: &network, ReadPaths: []string{"/tmp/gh"}, AllowEnv: []string{"GH_CONFIG_DIR"}}
	for _, old := range []string{"", "# keep\n[profiles.demo]\nmodel = '''a\n[sandbox]\nb'''\n", "# keep\n[sandbox] # old\nnetwork = 'allow'\nread_paths = [\n '/tmp/old',\n]\n[profiles.demo]\nmodel = 'keep'\n"} {
		got, err := UpdateSandbox([]byte(old), raw)
		if err != nil {
			t.Fatal(err)
		}
		if !strings.Contains(string(got), `network = 'deny'`) {
			t.Fatalf("%s", got)
		}
		if strings.Contains(old, "[profiles.demo]") && !strings.Contains(string(got), old[strings.Index(old, "[profiles.demo]"):]) {
			t.Fatalf("unrelated content changed: %s", got)
		}
	}
	if _, err := UpdateSandbox([]byte("sandbox.network = 'allow'\n"), raw); err == nil {
		t.Fatal("must reject unsupported dotted layout without overwriting it")
	}
}
