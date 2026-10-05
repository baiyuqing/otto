package shell

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const envMarker = "OTTO_ENV_BEGIN"

func ParseEnvironment(data []byte) ([]string, error) {
	_, after, found := bytes.Cut(data, []byte(envMarker))
	if !found {
		return nil, errors.New("the login shell's output had no OTTO_ENV_BEGIN marker")
	}
	var env []string
	for entry := range strings.SplitSeq(string(after), "\x00") {
		if key, _, ok := strings.Cut(entry, "="); ok && key != "" {
			env = append(env, entry)
		}
	}
	return env, nil
}

// macOS Directory Services is authoritative for the user's login shell.
// Neither SHELL nor a Finder launch's PATH is used to locate it.
func loginShell(ctx context.Context) (string, error) {
	if runtime.GOOS == "darwin" {
		user, err := exec.CommandContext(ctx, "/usr/bin/id", "-un").Output()
		if err != nil {
			return "", err
		}
		out, err := exec.CommandContext(ctx, "/usr/bin/dscl", ".", "-read", "/Users/"+strings.TrimSpace(string(user)), "UserShell").Output()
		if err != nil {
			return "", err
		}
		_, value, ok := strings.Cut(string(out), "UserShell:")
		if !ok || strings.TrimSpace(value) == "" {
			return "", errors.New("no login shell in Directory Services")
		}
		return strings.TrimSpace(value), nil
	}
	data, err := os.ReadFile("/etc/passwd")
	if err != nil {
		return "", err
	}
	for line := range strings.SplitSeq(string(data), "\n") {
		fields := strings.Split(line, ":")
		if len(fields) == 7 && fields[2] == strconv.Itoa(os.Getuid()) && fields[6] != "" {
			return fields[6], nil
		}
	}
	return "", errors.New("no login shell in the password database")
}

func captureShellEnvironment(ctx context.Context, shell string) ([]string, error) {
	cmd := exec.CommandContext(ctx, shell, "-l", "-i", "-c", "printf OTTO_ENV_BEGIN; env -0")
	// Kill the probe's process group on cancellation, including rc-file
	// children that could otherwise keep stdout open beyond the deadline.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	cmd.WaitDelay = time.Second
	data, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("capturing the login shell's environment: %w", err)
	}
	return ParseEnvironment(data)
}

// The fallback contains the app environment plus a reason for the native
// warning dialog. It never logs environment values.
func CaptureEnvironment(ctx context.Context) ([]string, error) {
	shell, err := loginShell(ctx)
	if err == nil {
		var env []string
		env, err = captureShellEnvironment(ctx, shell)
		if err == nil {
			return env, nil
		}
	}
	return os.Environ(), err
}
