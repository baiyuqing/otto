package shell

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"
)

type Child struct {
	cmd   *exec.Cmd
	stdin io.WriteCloser
	Lines <-chan string
	done  chan struct{}
	stop  sync.Once
	err   error // written before done closes; only read after receiving done
}

func Trust(ctx context.Context, binary, dir string, env []string) error {
	cmd := exec.CommandContext(ctx, binary, "trust", dir)
	cmd.Env = append([]string{}, env...)
	output, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("otto trust failed: %w\n\n%s", err, output)
	}
	return nil
}

func SpawnServe(binary, dir string, env []string, logPath string) (*Child, error) {
	if err := os.MkdirAll(filepath.Dir(logPath), 0700); err != nil {
		return nil, err
	}
	log, err := os.OpenFile(logPath, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
	if err != nil {
		return nil, err
	}
	defer log.Close()
	cmd := exec.Command(binary, "serve", "--cwd", dir, "--listen", "127.0.0.1:0", "--exit-on-stdin-close")
	cmd.Env = append([]string{}, env...)
	cmd.Stderr = log
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	// Our own pipe lets Wait reap the child immediately without racing a
	// StdoutPipe scanner and discarding a final announcement on fast exit.
	reader, writer, err := os.Pipe()
	if err != nil {
		stdin.Close()
		return nil, err
	}
	cmd.Stdout = writer
	err = cmd.Start()
	writer.Close()
	if err != nil {
		reader.Close()
		stdin.Close()
		return nil, err
	}
	lines := make(chan string, 16)
	child := &Child{cmd: cmd, stdin: stdin, Lines: lines, done: make(chan struct{})}
	go func() {
		child.err = cmd.Wait()
		close(child.done)
	}()
	go func() {
		defer reader.Close()
		defer close(lines)
		scanner := bufio.NewScanner(reader)
		for scanner.Scan() {
			select {
			case lines <- scanner.Text():
			case <-child.done:
				// Buffered startup lines still reach the caller after exit.
				select {
				case lines <- scanner.Text():
				default:
					return
				}
			}
		}
	}()
	return child, nil
}

func (c *Child) CloseStdin() error { return c.stdin.Close() }

func (c *Child) Wait(ctx context.Context) error {
	select {
	case <-c.done:
		return c.err
	case <-ctx.Done():
		return ctx.Err()
	}
}

// Idempotent cleanup also covers startup cancellation. Reap exactly once;
// keep stdin alive until shutdown so the server follows the same contract.
func (c *Child) Shutdown(grace time.Duration) {
	c.stop.Do(func() {
		defer c.stdin.Close()
		select {
		case <-c.done:
			return
		default:
		}
		_ = c.cmd.Process.Signal(syscall.SIGTERM)
		timer := time.NewTimer(grace)
		defer timer.Stop()
		select {
		case <-c.done:
		case <-timer.C:
			_ = c.cmd.Process.Kill()
			<-c.done
		}
	})
}

func DescribeStartupFailure(c *Child, reason error, logPath string) string {
	message := "Otto did not start: " + reason.Error()
	select {
	case <-c.done:
		if c.err != nil {
			message += "\n" + c.err.Error()
		} else {
			message += "\notto serve exited"
		}
	default:
	}
	if data, err := os.ReadFile(logPath); err == nil {
		lines := strings.Split(strings.TrimRight(string(data), "\n"), "\n")
		if len(lines) > 50 {
			lines = lines[len(lines)-50:]
		}
		if tail := strings.Join(lines, "\n"); tail != "" {
			message += "\n\n" + tail
		}
	}
	return message
}

// A controller owns the child even while startup is waiting for its URL.
// Stop cancels startup and atomically marks it closed before detaching the
// child. A concurrent Install after Stop must clean up the rejected child.
type Controller struct {
	mu     sync.Mutex
	child  *Child
	closed bool
	grace  time.Duration
}

func (c *Controller) Install(child *Child) error {
	c.mu.Lock()
	if c.closed {
		grace := c.grace
		c.mu.Unlock()
		child.Shutdown(grace)
		return context.Canceled
	}
	if c.child != nil {
		c.mu.Unlock()
		return errors.New("desktop child already installed")
	}
	c.child = child
	c.mu.Unlock()
	return nil
}

func (c *Controller) Stop(grace time.Duration) {
	c.mu.Lock()
	c.closed = true
	c.grace = grace
	child := c.child
	c.child = nil
	c.mu.Unlock()
	if child != nil {
		child.Shutdown(grace)
	}
}
