package config

import (
	"path/filepath"
	"strings"
	"testing"
)

func TestResolveServer(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	t.Setenv("HOME", "")
	env := map[string]string{"HOME": home}

	tests := []struct {
		name     string
		file     File
		override string
		env      map[string]string
		want     string
		wantErr  bool
	}{
		{
			name:     "override wins over file",
			file:     File{Server: Server{Socket: "/file/otto.sock"}},
			override: "/override/otto.sock",
			env:      env,
			want:     "/override/otto.sock",
		},
		{
			name: "file wins over default",
			file: File{Server: Server{Socket: "/file/otto.sock"}},
			env:  env,
			want: "/file/otto.sock",
		},
		{
			name: "default expands ~/ to env home",
			file: File{},
			env:  env,
			want: filepath.Join(home, ".otto", "otto.sock"),
		},
		{
			name:    "~/ with no home errors",
			file:    File{},
			env:     map[string]string{},
			wantErr: true,
		},
		{
			name: "plain absolute path is cleaned unchanged",
			file: File{Server: Server{Socket: "/x/y/../otto.sock"}},
			env:  env,
			want: "/x/otto.sock",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ResolveServer(tt.file, tt.env, tt.override)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("ResolveServer() err = nil, want error")
				}
				return
			}
			if err != nil {
				t.Fatalf("ResolveServer() err = %v, want nil", err)
			}
			if got.Socket != tt.want {
				t.Fatalf("ResolveServer().Socket = %q, want %q", got.Socket, tt.want)
			}
		})
	}
}

func TestLoadServerSection(t *testing.T) {
	path := writeConfig(t, `[server]
socket = "/x/otto.sock"
`)
	file, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	runtime, err := ResolveServer(file, map[string]string{}, "")
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Socket != "/x/otto.sock" {
		t.Fatalf("Socket = %q, want /x/otto.sock", runtime.Socket)
	}
}

func TestLoadServerRejectsUnknownFields(t *testing.T) {
	path := writeConfig(t, `[server]
socket = "/x/otto.sock"
unknown = true
`)
	if _, err := Load(path); err == nil || !strings.Contains(err.Error(), "unknown") {
		t.Fatalf("expected unknown-field error, got %v", err)
	}
}
