package tool

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"syscall"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

type BashSecurity struct {
	RemoveEnv    []string
	RedactValues []string
}

type bashTool struct {
	workspace      *Workspace
	shell          string
	timeout        time.Duration
	maxOutputBytes int
	removeEnv      map[string]struct{}
	redactValues   []string
}

type bashArgs struct {
	Command string `json:"command"`
}

func NewBashTool(workspace *Workspace, shell string, timeout time.Duration, maxOutputBytes int, securityOptions ...BashSecurity) Tool {
	tool := &bashTool{
		workspace:      workspace,
		shell:          shell,
		timeout:        timeout,
		maxOutputBytes: maxOutputBytes,
		removeEnv:      make(map[string]struct{}),
	}
	for _, security := range securityOptions {
		for _, name := range security.RemoveEnv {
			if name != "" {
				tool.removeEnv[name] = struct{}{}
			}
		}
		for _, value := range security.RedactValues {
			if value != "" {
				tool.redactValues = append(tool.redactValues, value)
			}
		}
	}
	return tool
}

func (t *bashTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "bash",
		Description: "Execute a shell command from the workspace",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"command": map[string]any{
					"type":        "string",
					"description": "Shell command to execute",
				},
			},
			"required": []string{"command"},
		},
	}
}

func (t *bashTool) Execute(ctx context.Context, arguments json.RawMessage) (result Result) {
	defer func() {
		for _, value := range t.redactValues {
			result.Content = strings.ReplaceAll(result.Content, value, "[REDACTED]")
		}
	}()
	var args bashArgs
	if err := decodeStrictJSON(arguments, &args, "command"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if strings.TrimSpace(args.Command) == "" {
		return Result{Content: "missing required argument: command", IsError: true}
	}
	if strings.TrimSpace(t.shell) == "" {
		return Result{Content: "invalid shell configuration", IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: formatBashResult(newCappedByteCollector(t.maxOutputBytes), newCappedByteCollector(t.maxOutputBytes), "status: cancelled"), IsError: false}
	}

	stdout := newCappedByteCollector(t.maxOutputBytes)
	stderr := newCappedByteCollector(t.maxOutputBytes)
	redactedStdout := newExactRedactingWriter(stdout, t.redactValues)
	redactedStderr := newExactRedactingWriter(stderr, t.redactValues)
	flushOutput := func() error {
		if err := redactedStdout.Flush(); err != nil {
			return err
		}
		return redactedStderr.Flush()
	}
	cmd := exec.Command(t.shell, "-lc", args.Command)
	cmd.Dir = t.workspace.root
	cmd.Env = filterEnvironment(os.Environ(), t.removeEnv)
	cmd.Stdout = redactedStdout
	cmd.Stderr = redactedStderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}

	if err := cmd.Start(); err != nil {
		return Result{Content: fmt.Sprintf("start shell command: %v", err), IsError: true}
	}

	waitCh := make(chan error, 1)
	go func() {
		waitCh <- cmd.Wait()
	}()

	var timeoutCh <-chan time.Time
	var timer *time.Timer
	if t.timeout > 0 {
		timer = time.NewTimer(t.timeout)
		timeoutCh = timer.C
		defer timer.Stop()
	}

	select {
	case err := <-waitCh:
		if flushErr := flushOutput(); flushErr != nil {
			return Result{Content: fmt.Sprintf("capture shell output: %v", flushErr), IsError: true}
		}
		return t.resultForExit(err, stdout, stderr)
	case <-ctx.Done():
		waitErr := t.killAndWait(cmd, waitCh)
		if waitErr != nil && !isProcessTermination(waitErr) {
			return Result{Content: fmt.Sprintf("cancel shell command: %v", waitErr), IsError: true}
		}
		if flushErr := flushOutput(); flushErr != nil {
			return Result{Content: fmt.Sprintf("capture shell output: %v", flushErr), IsError: true}
		}
		return Result{Content: formatBashResult(stdout, stderr, formatTerminationStatus("status: cancelled", waitErr)), IsError: false}
	case <-timeoutCh:
		waitErr := t.killAndWait(cmd, waitCh)
		if waitErr != nil && !isProcessTermination(waitErr) {
			return Result{Content: fmt.Sprintf("timeout shell command: %v", waitErr), IsError: true}
		}
		if flushErr := flushOutput(); flushErr != nil {
			return Result{Content: fmt.Sprintf("capture shell output: %v", flushErr), IsError: true}
		}
		return Result{Content: formatBashResult(stdout, stderr, formatTerminationStatus(fmt.Sprintf("status: timed out after %s", t.timeout), waitErr)), IsError: false}
	}
}

func (t *bashTool) killAndWait(cmd *exec.Cmd, waitCh <-chan error) error {
	killErr := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
	waitErr := <-waitCh
	if killErr != nil && !errors.Is(killErr, syscall.ESRCH) {
		return fmt.Errorf("kill process group: %w", killErr)
	}
	return waitErr
}

func (t *bashTool) resultForExit(err error, stdout, stderr *cappedByteCollector) Result {
	if err == nil {
		return Result{Content: formatBashResult(stdout, stderr, "exit_code: 0")}
	}

	var exitErr *exec.ExitError
	if errors.As(err, &exitErr) {
		return Result{Content: formatBashResult(stdout, stderr, fmt.Sprintf("exit_code: %d", exitErr.ExitCode()))}
	}

	return Result{Content: fmt.Sprintf("wait for shell command: %v", err), IsError: true}
}

func isProcessTermination(err error) bool {
	if err == nil {
		return false
	}
	var exitErr *exec.ExitError
	return errors.As(err, &exitErr)
}

func formatTerminationStatus(status string, err error) string {
	if signal := terminationSignal(err); signal != "" {
		return status + "; signal: " + signal
	}
	return status
}

func terminationSignal(err error) string {
	var exitErr *exec.ExitError
	if !errors.As(err, &exitErr) || exitErr.ProcessState == nil {
		return ""
	}
	ws, ok := exitErr.ProcessState.Sys().(syscall.WaitStatus)
	if !ok || !ws.Signaled() {
		return ""
	}
	return ws.Signal().String()
}

func filterEnvironment(environment []string, removed map[string]struct{}) []string {
	if len(removed) == 0 {
		return environment
	}
	filtered := make([]string, 0, len(environment))
	for _, entry := range environment {
		name, _, _ := strings.Cut(entry, "=")
		if _, remove := removed[name]; !remove {
			filtered = append(filtered, entry)
		}
	}
	return filtered
}

func formatBashResult(stdout, stderr *cappedByteCollector, status string) string {
	return strings.Join([]string{
		formatBashStream("stdout", stdout),
		formatBashStream("stderr", stderr),
		status,
	}, "\n")
}

func formatBashStream(name string, collector *cappedByteCollector) string {
	data := collector.Bytes()

	var builder strings.Builder
	builder.WriteString(name)
	builder.WriteString(":\n")
	builder.Write(data)
	if collector.Discarded() > 0 {
		if len(data) > 0 && data[len(data)-1] != '\n' {
			builder.WriteByte('\n')
		}
		builder.WriteString(fmt.Sprintf("[truncated: %d bytes omitted]", collector.Discarded()))
	}
	return builder.String()
}
