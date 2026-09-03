//go:build unix

package server

import (
	"fmt"
	"net"
	"os"
	"path/filepath"
	"syscall"
)

// Listen creates a Unix domain socket listener at path. The parent directory
// is created with mode 0700 if missing; if it already exists it must be
// owned by the current user and not group- or world-accessible. A leftover
// socket file at path is dialed to tell a live server (rejected as "already
// running") from a stale one (removed and replaced).
func Listen(path string) (net.Listener, error) {
	if err := ensureSocketDirectory(filepath.Dir(path)); err != nil {
		return nil, err
	}
	if err := removeStaleSocket(path); err != nil {
		return nil, err
	}

	listener, err := net.Listen("unix", path)
	if err != nil {
		return nil, fmt.Errorf("listen on %s: %w", path, err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		_ = listener.Close()
		return nil, fmt.Errorf("chmod %s: %w", path, err)
	}
	return listener, nil
}

func ensureSocketDirectory(dir string) error {
	info, err := os.Stat(dir)
	if err != nil {
		if os.IsNotExist(err) {
			if err := os.MkdirAll(dir, 0o700); err != nil {
				return fmt.Errorf("create socket directory %s: %w", dir, err)
			}
			return nil
		}
		return fmt.Errorf("stat socket directory %s: %w", dir, err)
	}
	if !info.IsDir() {
		return fmt.Errorf("socket directory %s is not a directory", dir)
	}
	if info.Mode().Perm()&0o077 != 0 {
		return fmt.Errorf("socket directory %s must not be group- or world-accessible", dir)
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok || stat.Uid != currentUID() {
		return fmt.Errorf("socket directory %s must be owned by the current user", dir)
	}
	return nil
}

func currentUID() uint32 {
	return uint32(os.Geteuid())
}

func removeStaleSocket(path string) error {
	if _, err := os.Stat(path); err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return fmt.Errorf("stat socket %s: %w", path, err)
	}

	conn, dialErr := net.Dial("unix", path)
	if dialErr == nil {
		_ = conn.Close()
		return fmt.Errorf("already running at %s", path)
	}
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove stale socket %s: %w", path, err)
	}
	return nil
}
