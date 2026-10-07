package ottoconfig

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

const sample = `
default_profile = "deepseek"

[ui]
mode = ""

[profiles.deepseek]
provider = "openai-compatible"
base_url = "https://api.deepseek.example/v1"
model = "deepseek-v4-flash"
api_key_env = "DEEPSEEK_API_KEY"

[profiles."chatgpt-5.5"]
provider = "chatgpt"
model = "gpt-5.5"

[profiles.terra]
provider = "chatgpt"
model = "gpt-5.6-terra"

[server]
socket = ""
`

func write(t *testing.T, text string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestReadParsesSortedProfilesAndTheDefault(t *testing.T) {
	t.Parallel()
	snapshot, err := Read(write(t, sample))
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Default != "deepseek" {
		t.Errorf("default = %q, want deepseek", snapshot.Default)
	}
	want := []Profile{
		{Name: "chatgpt-5.5", Provider: "chatgpt", Model: "gpt-5.5"},
		{Name: "deepseek", Provider: "openai-compatible", Model: "deepseek-v4-flash"},
		{Name: "terra", Provider: "chatgpt", Model: "gpt-5.6-terra"},
	}
	if !reflect.DeepEqual(snapshot.Profiles, want) {
		t.Errorf("profiles = %+v, want %+v", snapshot.Profiles, want)
	}
}

func TestReadMissingFileIsAnEmptySnapshot(t *testing.T) {
	t.Parallel()
	snapshot, err := Read(filepath.Join(t.TempDir(), "config.toml"))
	if err != nil {
		t.Fatalf("a missing file must not be an error: %v", err)
	}
	if snapshot.Default != "" || len(snapshot.Profiles) != 0 {
		t.Errorf("snapshot = %+v, want empty", snapshot)
	}
}

func TestReadRejectsBrokenTOML(t *testing.T) {
	t.Parallel()
	if _, err := Read(write(t, "[profiles.deepseek\nmodel = ")); err == nil {
		t.Fatal("a malformed file must be an error")
	}
}

func TestPathIsTheUserConfiguration(t *testing.T) {
	t.Parallel()
	if got, want := Path("/home/me"), "/home/me/.config/otto/config.toml"; got != want {
		t.Errorf("Path = %q, want %q", got, want)
	}
}
