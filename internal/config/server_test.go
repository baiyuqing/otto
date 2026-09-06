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
		name           string
		file           File
		socketOverride string
		listenOverride string
		env            map[string]string
		want           ServerRuntime
		wantErr        bool
	}{
		{
			name:           "socket override wins over file",
			file:           File{Server: Server{Socket: "/file/otto.sock"}},
			socketOverride: "/override/otto.sock",
			env:            env,
			want:           ServerRuntime{Socket: "/override/otto.sock"},
		},
		{
			name: "file wins over default",
			file: File{Server: Server{Socket: "/file/otto.sock"}},
			env:  env,
			want: ServerRuntime{Socket: "/file/otto.sock"},
		},
		{
			name: "default expands ~/ to env home",
			file: File{},
			env:  env,
			want: ServerRuntime{Socket: filepath.Join(home, ".otto", "otto.sock")},
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
			want: ServerRuntime{Socket: "/x/otto.sock"},
		},
		{
			name:           "listen override wins over everything",
			file:           File{Server: Server{Socket: "/file/otto.sock", Listen: "127.0.0.1:1"}},
			socketOverride: "/override/otto.sock",
			listenOverride: "127.0.0.1:2",
			env:            env,
			want:           ServerRuntime{Listen: "127.0.0.1:2"},
		},
		{
			name:           "socket override wins over file listen",
			file:           File{Server: Server{Listen: "127.0.0.1:1"}},
			socketOverride: "/override/otto.sock",
			env:            env,
			want:           ServerRuntime{Socket: "/override/otto.sock"},
		},
		{
			name: "file listen wins over file socket and leaves Socket empty",
			file: File{Server: Server{Socket: "/file/otto.sock", Listen: "127.0.0.1:1"}},
			env:  env,
			want: ServerRuntime{Listen: "127.0.0.1:1"},
		},
		{
			name: "file listen needs no home",
			file: File{Server: Server{Listen: "127.0.0.1:1"}},
			env:  map[string]string{},
			want: ServerRuntime{Listen: "127.0.0.1:1"},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ResolveServer(tt.file, tt.env, tt.socketOverride, tt.listenOverride)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("ResolveServer() err = nil, want error")
				}
				return
			}
			if err != nil {
				t.Fatalf("ResolveServer() err = %v, want nil", err)
			}
			if got != tt.want {
				t.Fatalf("ResolveServer() = %+v, want %+v", got, tt.want)
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
	runtime, err := ResolveServer(file, map[string]string{}, "", "")
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Socket != "/x/otto.sock" {
		t.Fatalf("Socket = %q, want /x/otto.sock", runtime.Socket)
	}
}

func TestLoadServerListen(t *testing.T) {
	path := writeConfig(t, `[server]
listen = "127.0.0.1:0"
`)
	file, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	runtime, err := ResolveServer(file, map[string]string{}, "", "")
	if err != nil {
		t.Fatal(err)
	}
	if runtime != (ServerRuntime{Listen: "127.0.0.1:0"}) {
		t.Fatalf("runtime = %+v, want Listen 127.0.0.1:0 only", runtime)
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
