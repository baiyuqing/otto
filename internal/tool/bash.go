package tool

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/sandbox"
)

type bashArgs struct {
	Command string `json:"command"`
}

var (
	errInvalidSandboxedBashConfiguration = errors.New("invalid sandboxed bash configuration")
	errSandboxedBashTimeout              = errors.New("sandboxed bash timeout")
)

const (
	sandboxExecutionUnavailable     = "sandbox execution unavailable"
	preferredSandboxRedactionMarker = '*'
)

type bashTool struct {
	workspace       *Workspace
	executor        sandbox.CommandExecutor
	shell           string
	environment     []string
	timeout         time.Duration
	maxOutputBytes  int
	redactValues    []string
	redactionMarker string

	deadlineContext func(context.Context, time.Duration, error) (context.Context, context.CancelFunc)
}

func NewBashTool(
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

	redactValues := cloneSandboxedBashStrings(redactionValues)
	redactionMarker, ok := collisionSafeSandboxRedactionMarker(redactValues)
	if !ok {
		return nil, errInvalidSandboxedBashConfiguration
	}

	return &bashTool{
		workspace: &Workspace{
			root:        strings.Clone(workspace.root),
			lexicalRoot: strings.Clone(workspace.lexicalRoot),
		},
		executor:        executor,
		shell:           strings.Clone(shell),
		environment:     cloneSandboxedBashStrings(environment),
		timeout:         timeout,
		maxOutputBytes:  maxOutputBytes,
		redactValues:    redactValues,
		redactionMarker: redactionMarker,
		deadlineContext: context.WithTimeoutCause,
	}, nil
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

func (t *bashTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args bashArgs
	if err := decodeStrictJSON(arguments, &args, "command"); err != nil {
		return t.sandboxedArgumentError(err.Error())
	}
	if strings.TrimSpace(args.Command) == "" {
		return t.sandboxedArgumentError("missing required argument: command")
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
	redactedStdout := newExactRedactingWriterWithMarker(stdout, t.redactValues, t.redactionMarker)
	redactedStderr := newExactRedactingWriterWithMarker(stderr, t.redactValues, t.redactionMarker)

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

func (t *bashTool) sandboxedArgumentError(message string) Result {
	redacted, err := redactExactText(message, t.redactValues, t.redactionMarker)
	if err != nil {
		return sandboxedBashInfrastructureResult()
	}
	return Result{Content: redacted, IsError: true}
}

func (t *bashTool) sandboxedResult(stdout, stderr *cappedByteCollector, status sandbox.ExitStatus, summary string) Result {
	if status.Signaled && status.Signal != "" {
		summary += "; signal: " + status.Signal
	}
	redactedSummary, err := redactExactText(summary, t.redactValues, t.redactionMarker)
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

func collisionSafeSandboxRedactionMarker(values []string) (string, bool) {
	var usedBytes [256]bool
	for _, value := range values {
		if value == "" {
			continue
		}
		for _, valueByte := range []byte(value) {
			usedBytes[valueByte] = true
		}
	}

	isSafe := func(candidate rune) (string, bool) {
		if !utf8.ValidRune(candidate) || unicode.IsControl(candidate) {
			return "", false
		}
		var encoded [utf8.UTFMax]byte
		length := utf8.EncodeRune(encoded[:], candidate)
		for _, candidateByte := range encoded[:length] {
			if usedBytes[candidateByte] {
				return "", false
			}
		}
		return string(encoded[:length]), true
	}

	if marker, ok := isSafe(preferredSandboxRedactionMarker); ok {
		return marker, true
	}
	for candidate := rune('!'); candidate <= '~'; candidate++ {
		if candidate == preferredSandboxRedactionMarker {
			continue
		}
		if marker, ok := isSafe(candidate); ok {
			return marker, true
		}
	}
	for candidate := rune(utf8.RuneSelf); candidate <= utf8.MaxRune; candidate++ {
		if !unicode.IsGraphic(candidate) || unicode.IsSpace(candidate) {
			continue
		}
		if marker, ok := isSafe(candidate); ok {
			return marker, true
		}
	}
	for candidate := rune(utf8.RuneSelf); candidate <= utf8.MaxRune; candidate++ {
		if marker, ok := isSafe(candidate); ok {
			return marker, true
		}
	}
	return isSafe(' ')
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
