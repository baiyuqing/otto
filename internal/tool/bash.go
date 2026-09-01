package tool

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/safetext"
	"github.com/baiyuqing/otto/internal/sandbox"
)

type bashArgs struct {
	Command string `json:"command"`
}

var (
	errInvalidSandboxedBashConfiguration = errors.New("invalid sandboxed bash configuration")
	errSandboxedBashTimeout              = errors.New("sandboxed bash timeout")
)

const sandboxExecutionUnavailable = "sandbox execution unavailable"

type bashTool struct {
	workspace       *Workspace
	executor        sandbox.CommandExecutor
	shell           string
	environment     []string
	timeout         time.Duration
	maxOutputBytes  int
	redactValues    []string
	redactionMarker string
	dynamicContent  bool

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

	redactValues := canonicalizeSandboxedBashRedactions(redactionValues)
	redactionMarker, dynamicContent := safetext.DynamicRedactionMarker(redactValues)
	if !dynamicContent {
		redactValues = nil
		redactionMarker = ""
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
		dynamicContent:  dynamicContent,
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
	if !t.dynamicContent {
		return t.executeWithoutDynamicContent(ctx, args.Command)
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
	normalizedStdout := newUTF8NormalizingWriter(redactedStdout)
	normalizedStderr := newUTF8NormalizingWriter(redactedStderr)

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
	}, sandbox.Streams{Stdout: normalizedStdout, Stderr: normalizedStderr})

	cancel()
	commandCause := context.Cause(commandCtx)
	parentErr := ctx.Err()

	exactContextException := executionErr == context.Canceled || executionErr == context.DeadlineExceeded
	if executionErr != nil && !exactContextException {
		return sandboxedBashInfrastructureResult()
	}
	if err := normalizedStdout.Flush(); err != nil {
		return sandboxedBashInfrastructureResult()
	}
	if err := normalizedStderr.Flush(); err != nil {
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

func (t *bashTool) executeWithoutDynamicContent(ctx context.Context, command string) Result {
	if isNilSandboxedBashBoundary(ctx) || ctx.Err() != nil {
		return Result{}
	}
	commandCtx, cancel := t.deadlineContext(ctx, t.timeout, errSandboxedBashTimeout)
	if isNilSandboxedBashBoundary(commandCtx) || cancel == nil {
		if cancel != nil {
			cancel()
		}
		return Result{IsError: true}
	}
	_, executionErr := t.executor.Execute(commandCtx, sandbox.Request{
		Argv: cloneSandboxedBashStrings([]string{t.shell, "-lc", command}),
		Dir:  t.workspace.root,
		Env:  cloneSandboxedBashStrings(t.environment),
	}, sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard})
	cancel()
	if executionErr != nil && executionErr != context.Canceled && executionErr != context.DeadlineExceeded {
		return Result{IsError: true}
	}
	return Result{}
}

func (t *bashTool) sandboxedArgumentError(message string) Result {
	if !t.dynamicContent {
		return Result{IsError: true}
	}
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
	formatted := formatBashResult(stdout, stderr, summary)
	redacted, err := redactExactText(formatted, t.redactValues, t.redactionMarker)
	if err != nil {
		return sandboxedBashInfrastructureResult()
	}
	return Result{Content: redacted}
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

func canonicalizeSandboxedBashRedactions(values []string) []string {
	if values == nil {
		return nil
	}
	canonical := make([]string, 0, len(values))
	seen := make(map[string]struct{}, len(values))
	for _, value := range values {
		for _, form := range safetext.SecretForms(value) {
			if _, duplicate := seen[form]; duplicate {
				continue
			}
			seen[form] = struct{}{}
			canonical = append(canonical, strings.Clone(form))
		}
	}
	return canonical
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
