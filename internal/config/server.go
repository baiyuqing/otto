package config

import (
	"fmt"
	"path/filepath"
	"strings"
)

type Server struct {
	Socket string `toml:"socket"`
}

// ServerRuntime is the resolved [server] configuration for one process.
type ServerRuntime struct {
	Socket string // absolute path
}

const defaultServerSocket = "~/.otto/otto.sock"

// ResolveServer picks the socket path: override (the --socket flag) >
// file.Server.Socket > defaultServerSocket. A relative path is only cleaned,
// not resolved against a directory; the caller resolves it against the
// process cwd.
func ResolveServer(file File, env map[string]string, override string) (ServerRuntime, error) {
	socket := override
	if socket == "" {
		socket = file.Server.Socket
	}
	if socket == "" {
		socket = defaultServerSocket
	}

	if !strings.HasPrefix(socket, "~/") {
		return ServerRuntime{Socket: filepath.Clean(socket)}, nil
	}

	home := homeFromEnv(env)
	if home == "" {
		return ServerRuntime{}, fmt.Errorf("resolve home directory for server socket %q", socket)
	}
	return ServerRuntime{Socket: filepath.Clean(filepath.Join(home, strings.TrimPrefix(socket, "~/")))}, nil
}
