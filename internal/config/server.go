package config

import (
	"fmt"
	"path/filepath"
	"strings"
)

type Server struct {
	Socket string `toml:"socket"`
	Listen string `toml:"listen"`
}

// ServerRuntime is the resolved [server] configuration for one process.
// Exactly one of Listen and Socket is set: a non-empty Listen means the
// server binds a loopback TCP address and serves no Unix socket.
type ServerRuntime struct {
	Socket string // absolute path
	Listen string // host:port
}

const defaultServerSocket = "~/.otto/otto.sock"

// ResolveServer picks the listener: listenOverride (--listen) >
// socketOverride (--socket) > file.Server.Listen > file.Server.Socket >
// defaultServerSocket. A relative socket path is only cleaned, not resolved
// against a directory; the caller resolves it against the process cwd.
func ResolveServer(file File, env map[string]string, socketOverride, listenOverride string) (ServerRuntime, error) {
	switch {
	case listenOverride != "":
		return ServerRuntime{Listen: listenOverride}, nil
	case socketOverride != "":
		return resolveSocket(socketOverride, env)
	case file.Server.Listen != "":
		return ServerRuntime{Listen: file.Server.Listen}, nil
	case file.Server.Socket != "":
		return resolveSocket(file.Server.Socket, env)
	}
	return resolveSocket(defaultServerSocket, env)
}

func resolveSocket(socket string, env map[string]string) (ServerRuntime, error) {
	if !strings.HasPrefix(socket, "~/") {
		return ServerRuntime{Socket: filepath.Clean(socket)}, nil
	}

	home := homeFromEnv(env)
	if home == "" {
		return ServerRuntime{}, fmt.Errorf("resolve home directory for server socket %q", socket)
	}
	return ServerRuntime{Socket: filepath.Clean(filepath.Join(home, strings.TrimPrefix(socket, "~/")))}, nil
}
