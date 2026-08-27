package config

import (
	"strings"
	"testing"
)

func TestResolveUIModePrecedence(t *testing.T) {
	file := File{UI: UI{Mode: "repl"}}
	tests := []struct {
		name     string
		env      map[string]string
		override string
		want     UIMode
	}{
		{name: "toml", want: UIRepl},
		{name: "environment", env: map[string]string{"OTTO_UI": "tui"}, want: UITUI},
		{name: "cli", env: map[string]string{"OTTO_UI": "tui"}, override: "auto", want: UIAuto},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got, err := ResolveUIMode(file, test.env, test.override)
			if err != nil {
				t.Fatal(err)
			}
			if got != test.want {
				t.Fatalf("mode = %q, want %q", got, test.want)
			}
		})
	}
}

func TestResolveUIModeRejectsUnknownValue(t *testing.T) {
	_, err := ResolveUIMode(File{}, map[string]string{"OTTO_UI": "graphical"}, "")
	if err == nil || !strings.Contains(err.Error(), "auto, tui, repl") {
		t.Fatalf("unexpected error: %v", err)
	}
}
