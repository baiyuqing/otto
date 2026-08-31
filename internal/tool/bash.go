package tool

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"syscall"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/sandbox"
)

// BashSecurity and bashTool are the temporary process-backed implementation.
// Task 8 removes them after all production call sites migrate atomically.
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

var (
	errInvalidSandboxedBashConfiguration = errors.New("invalid sandboxed bash configuration")
	errSandboxedBashTimeout              = errors.New("sandboxed bash timeout")
)

const sandboxExecutionUnavailable = "sandbox execution unavailable"

type sandboxedBashTool struct {
	workspace      *Workspace
	executor       sandbox.CommandExecutor
	shell          string
	environment    []string
	timeout        time.Duration
	maxOutputBytes int
	redactValues   []string

	deadlineContext func(context.Context, time.Duration, error) (context.Context, context.CancelFunc)
}

func NewSandboxedBashTool(
	workspace *Workspace,
	executor sandbox.CommandExecutor,
	shell string,
	environment []string,
	timeout time.Duration,
	maxOutputBytes int,
	redactionValues []string,
) (Tool, error) {
	if !validSandboxedBashWorkspace(workspace) ||
		isNilSandboxedBashBoundary(executor) ||
		strings.TrimSpace(shell) == "" ||
		strings.IndexByte(shell, 0) >= 0 ||
		environment == nil ||
		timeout <= 0 ||
		maxOutputBytes <= 0 {
		return nil, errInvalidSandboxedBashConfiguration
	}

	return &sandboxedBashTool{
		workspace: &Workspace{
			root:        strings.Clone(workspace.root),
			lexicalRoot: strings.Clone(workspace.lexicalRoot),
		},
		executor:        executor,
		shell:           strings.Clone(shell),
		environment:     cloneSandboxedBashStrings(environment),
		timeout:         timeout,
		maxOutputBytes:  maxOutputBytes,
		redactValues:    cloneSandboxedBashStrings(redactionValues),
		deadlineContext: context.WithTimeoutCause,
	}, nil
}

func (t *sandboxedBashTool) Definition() model.ToolDefinition {
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

func (t *sandboxedBashTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args bashArgs
	if err := decodeStrictJSON(arguments, &args, "command"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if strings.TrimSpace(args.Command) == "" {
		return Result{Content: "missing required argument: command", IsError: true}
	}
	if isNilSandboxedBashBoundary(ctx) {
		return sandboxedBashInfrastructureResult()
	}
	if ctx.Err() != nil {
		return t.sandboxedResult(
			newCappedByteCollector(t.maxOutputBytes),
			newCappedByteCollector(t.maxOutputBytes),
			sandbox.ExitStatus{},
			"status: cancelled",
		)
	}

	stdout := newCappedByteCollector(t.maxOutputBytes)
	stderr := newCappedByteCollector(t.maxOutputBytes)
	redactedStdout := newExactRedactingWriter(stdout, t.redactValues)
	redactedStderr := newExactRedactingWriter(stderr, t.redactValues)

	commandCtx, cancel := t.deadlineContext(ctx, t.timeout, errSandboxedBashTimeout)
	if isNilSandboxedBashBoundary(commandCtx) || cancel == nil {
		if cancel != nil {
			cancel()
		}
		return sandboxedBashInfrastructureResult()
	}
	status, executionErr := t.executor.Execute(commandCtx, sandbox.Request{
		Argv: cloneSandboxedBashStrings([]string{t.shell, "-lc", args.Command}),
		Dir:  t.workspace.root,
		Env:  cloneSandboxedBashStrings(t.environment),
	}, sandbox.Streams{Stdout: redactedStdout, Stderr: redactedStderr})

	cancel()
	commandCause := context.Cause(commandCtx)
	parentErr := ctx.Err()

	exactContextException := executionErr == context.Canceled || executionErr == context.DeadlineExceeded
	if executionErr != nil && !exactContextException {
		return sandboxedBashInfrastructureResult()
	}
	if err := redactedStdout.Flush(); err != nil {
		return sandboxedBashInfrastructureResult()
	}
	if err := redactedStderr.Flush(); err != nil {
		return sandboxedBashInfrastructureResult()
	}

	switch {
	case parentErr != nil:
		return t.sandboxedResult(stdout, stderr, status, "status: cancelled")
	case commandCause == errSandboxedBashTimeout:
		return t.sandboxedResult(stdout, stderr, status, fmt.Sprintf("status: timed out after %s", t.timeout))
	case executionErr == context.DeadlineExceeded:
		return t.sandboxedResult(stdout, stderr, status, fmt.Sprintf("status: timed out after %s", t.timeout))
	case executionErr == context.Canceled || commandCause != nil && commandCause != context.Canceled:
		return t.sandboxedResult(stdout, stderr, status, "status: cancelled")
	default:
		return t.sandboxedResult(stdout, stderr, status, fmt.Sprintf("exit_code: %d", status.Code))
	}
}

func (t *sandboxedBashTool) sandboxedResult(stdout, stderr *cappedByteCollector, status sandbox.ExitStatus, summary string) Result {
	if status.Signaled && status.Signal != "" {
		summary += "; signal: " + status.Signal
	}
	redactedSummary, err := redactExactText(summary, t.redactValues)
	if err != nil {
		return sandboxedBashInfrastructureResult()
	}
	return Result{Content: formatBashResult(stdout, stderr, redactedSummary)}
}

func sandboxedBashInfrastructureResult() Result {
	return Result{Content: sandboxExecutionUnavailable, IsError: true}
}

func validSandboxedBashWorkspace(workspace *Workspace) bool {
	if workspace == nil ||
		workspace.root == "" ||
		workspace.lexicalRoot == "" ||
		strings.IndexByte(workspace.root, 0) >= 0 ||
		strings.IndexByte(workspace.lexicalRoot, 0) >= 0 ||
		!filepath.IsAbs(workspace.root) ||
		!filepath.IsAbs(workspace.lexicalRoot) ||
		filepath.Clean(workspace.root) != workspace.root ||
		filepath.Clean(workspace.lexicalRoot) != workspace.lexicalRoot {
		return false
	}
	resolvedRoot, err := filepath.EvalSymlinks(workspace.root)
	if err != nil || resolvedRoot != workspace.root {
		return false
	}
	info, err := os.Stat(resolvedRoot)
	if err != nil || !info.IsDir() {
		return false
	}
	resolvedLexicalRoot, err := filepath.EvalSymlinks(workspace.lexicalRoot)
	return err == nil && resolvedLexicalRoot == workspace.root
}

func isNilSandboxedBashBoundary(value any) bool {
	if value == nil {
		return true
	}
	reflected := reflect.ValueOf(value)
	switch reflected.Kind() {
	case reflect.Chan, reflect.Func, reflect.Interface, reflect.Map, reflect.Pointer, reflect.Slice:
		return reflected.IsNil()
	default:
		return false
	}
}

func cloneSandboxedBashStrings(values []string) []string {
	if values == nil {
		return nil
	}
	cloned := make([]string, len(values))
	for i, value := range values {
		cloned[i] = strings.Clone(value)
	}
	return cloned
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
