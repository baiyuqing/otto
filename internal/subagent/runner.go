// Package subagent implements Otto's sub-agent tasks: a Runner starts
// child agent.Agent loops in their own goroutines, tracks their progress in
// the parent's agent.Tasks registry, and exposes the parent-facing agent,
// agent_wait, and agent_status tools.
package subagent

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

// Config holds everything a Runner needs from the parent runtime.
type Config struct {
	Provider provider.Provider
	// Tools is the parent's tool set; the Runner drops ExcludedChildTools by
	// name to build each child's registry.
	Tools []tool.Tool
	// Redactor is shared with the parent and is never mutated by a child.
	Redactor *agent.Redactor
	// Options is a template: Model, Thinking, Compaction, RequestSizer, Now,
	// and NewID are copied into every child's options. Memory, Inbox, Tasks,
	// and SystemPrompt are ignored; the Runner sets those per child.
	Options agent.Options
	// PromptFor renders the parent's static system prompt for a tool set.
	// Its output is treated as already redacted by the caller.
	PromptFor func(defs []model.ToolDefinition) string
	// Header is the parent session header; each child's header is a copy
	// with ID = Header.ID + "-" + taskID and CreatedAt = now.
	Header session.Header
	// Tasks is the session's task registry.
	Tasks *agent.Tasks
	// MaxParallel caps concurrent children; <= 0 means 4.
	MaxParallel int
	// MaxOutputBytes caps the report text inside notification/wait text;
	// <= 0 means 16384.
	MaxOutputBytes int
}

// ExcludedChildTools are never given to a child: the agent-control tools (a
// child never starts its own children, depth is fixed at 1) and the memory
// tools (children get no memory binding).
var ExcludedChildTools = []string{"agent", "agent_wait", "agent_status", "agent_send", "agent_cancel", "remember", "forget", "memory_search"}

// genericSubagentInstruction is appended to a child's system prompt. Named
// agent definitions do not exist yet in this phase, so every child gets
// this instruction.
const genericSubagentInstruction = "You are running as a sub-agent of Otto. Complete only the delegated task below with the available tools, then reply with a self-contained final report. That final message is returned to the caller as your result; nothing else you write is."

const (
	defaultMaxParallel    = 4
	defaultMaxOutputBytes = 16384
	maxDescriptionRunes   = 80
)

// Runner builds and runs sub-agent tasks for one session.
type Runner struct {
	config        Config
	childRegistry *tool.Registry
	sem           chan struct{}
}

// NewRunner validates config and builds the child tool registry once.
func NewRunner(config Config) (*Runner, error) {
	if config.Provider == nil {
		return nil, errors.New("subagent: Provider is required")
	}
	if config.Tasks == nil {
		return nil, errors.New("subagent: Tasks is required")
	}
	if config.PromptFor == nil {
		return nil, errors.New("subagent: PromptFor is required")
	}
	if config.MaxParallel <= 0 {
		config.MaxParallel = defaultMaxParallel
	}
	if config.MaxOutputBytes <= 0 {
		config.MaxOutputBytes = defaultMaxOutputBytes
	}

	excluded := make(map[string]struct{}, len(ExcludedChildTools))
	for _, name := range ExcludedChildTools {
		excluded[name] = struct{}{}
	}
	childTools := make([]tool.Tool, 0, len(config.Tools))
	for _, t := range config.Tools {
		if _, skip := excluded[t.Definition().Name]; skip {
			continue
		}
		childTools = append(childTools, t)
	}
	childRegistry, err := tool.NewRegistry(childTools...)
	if err != nil {
		return nil, fmt.Errorf("subagent: child registry: %w", err)
	}

	return &Runner{
		config:        config,
		childRegistry: childRegistry,
		sem:           make(chan struct{}, config.MaxParallel),
	}, nil
}

// StartRequest is one delegation request.
type StartRequest struct {
	Prompt      string
	Description string
	// Model is the provider model id the child runs on. Empty (or
	// whitespace-only) falls back to Config.Options.Model.
	Model string
}

// Start registers a task and starts its child goroutine, or leaves it
// queued when MaxParallel children are already running.
func (r *Runner) Start(request StartRequest) (agent.Task, error) {
	now := r.now()
	description := truncateRunesEllipsis(strings.TrimSpace(request.Description), maxDescriptionRunes)
	effectiveModel := strings.TrimSpace(request.Model)
	if effectiveModel == "" {
		effectiveModel = r.config.Options.Model
	}

	taskCtx, cancel := context.WithCancel(context.Background())

	// The child session is created after Tasks.Add assigns the task id (the
	// child header embeds it), but Add needs a history func now. Add
	// publishes the task (and this closure) to other goroutines before the
	// memory is constructed below, so a plain *session.Memory variable
	// would race a concurrent Tasks.History call against that later
	// assignment; the atomic pointer makes the handoff safe.
	var childMemory atomic.Pointer[session.Memory]
	history := func() []model.Message {
		if m := childMemory.Load(); m != nil {
			return m.Messages()
		}
		return nil
	}

	task, err := r.config.Tasks.Add(agent.Task{
		Description: description,
		Prompt:      request.Prompt,
		Context:     "fresh",
		Model:       effectiveModel,
		CreatedAt:   now,
	}, cancel, history)
	if err != nil {
		cancel()
		return agent.Task{}, err
	}

	childHeader := r.config.Header
	childHeader.ID = r.config.Header.ID + "-" + task.ID
	childHeader.CreatedAt = now
	mem := session.NewMemory(childHeader)
	childMemory.Store(mem)

	systemPrompt := r.config.Redactor.RedactString(
		r.config.PromptFor(r.childRegistry.Definitions()) + "\n\n## Sub-agent role\n" + genericSubagentInstruction,
	)
	childOptions := r.config.Options
	childOptions.SystemPrompt = systemPrompt
	childOptions.Model = r.config.Redactor.RedactString(effectiveModel)
	childOptions.Thinking = r.config.Redactor.RedactString(r.config.Options.Thinking)
	childOptions.Inbox = agent.NewInbox(nil)
	childOptions.Tasks = nil
	childOptions.Memory = nil

	child := agent.New(r.config.Provider, r.childRegistry, mem, childOptions, r.config.Redactor)

	go r.runChild(taskCtx, cancel, task.ID, request.Prompt, child, mem)

	return task, nil
}

// runChild waits for a semaphore slot (or cancellation while queued), runs
// the child to completion, and always finishes the task record.
func (r *Runner) runChild(taskCtx context.Context, cancel context.CancelFunc, taskID, prompt string, child *agent.Agent, childMemory *session.Memory) {
	defer cancel()

	select {
	case r.sem <- struct{}{}:
	case <-taskCtx.Done():
		r.finish(taskID, agent.TaskCanceled, r.now(), childMemory.Messages(), nil)
		return
	}

	r.config.Tasks.Update(taskID, func(t *agent.Task) {
		t.Status = agent.TaskRunning
		t.StartedAt = r.now()
	})

	progress := newChildProgress(r.config.Tasks, taskID, r.now)
	runErr := child.Run(taskCtx, prompt, progress.handle)
	<-r.sem
	child.Close()

	status := agent.TaskSucceeded
	switch {
	case runErr == nil:
		status = agent.TaskSucceeded
	case errors.Is(runErr, context.Canceled), taskCtx.Err() != nil:
		status = agent.TaskCanceled
	default:
		status = agent.TaskFailed
	}
	r.finish(taskID, status, r.now(), childMemory.Messages(), runErr)
}

// finish marks a task final and pushes its completion notification. The
// notification is pushed before the registry update that closes the task's
// Wait channel, so a caller unblocked by Wait always finds the notification
// already in the inbox.
func (r *Runner) finish(taskID string, status agent.TaskStatus, finishedAt time.Time, messages []model.Message, runErr error) {
	result := lastAssistantText(messages)
	if result == "" {
		result = "(sub-agent returned no final text)"
	}
	errText := ""
	if status == agent.TaskFailed && runErr != nil {
		errText = runErr.Error()
	}

	current, _ := r.config.Tasks.Get(taskID)
	final := current
	final.Status = status
	final.FinishedAt = finishedAt
	final.Result = result
	final.Error = errText

	usage := final.Usage
	r.config.Tasks.Notifications().Push(agent.Notification{
		TaskID: taskID,
		Kind:   agent.NotificationTaskFinished,
		Text:   CompletionText(final, r.config.MaxOutputBytes),
		Usage:  &usage,
	})

	r.config.Tasks.Update(taskID, func(t *agent.Task) {
		t.Status = status
		t.FinishedAt = finishedAt
		t.Result = result
		t.Error = errText
	})
}

func (r *Runner) now() time.Time {
	if r.config.Options.Now != nil {
		return r.config.Options.Now()
	}
	return time.Now().UTC()
}

// childProgress turns a child's emitted events into task record updates.
type childProgress struct {
	tasks   *agent.Tasks
	taskID  string
	now     func() time.Time
	textBuf strings.Builder
}

func newChildProgress(tasks *agent.Tasks, taskID string, now func() time.Time) *childProgress {
	return &childProgress{tasks: tasks, taskID: taskID, now: now}
}

// handle is the child's emit callback. It never forwards events to the
// parent's frontend; it only updates the task record.
func (p *childProgress) handle(event agent.Event) {
	switch event.Type {
	case agent.EventTextDelta:
		p.textBuf.WriteString(event.Text)
	case agent.EventProviderUsage:
		text := capLastBytes(p.textBuf.String(), 500)
		p.textBuf.Reset()
		p.tasks.Update(p.taskID, func(t *agent.Task) {
			t.Steps++
			t.Usage.InputTokens += event.Usage.InputTokens
			t.Usage.OutputTokens += event.Usage.OutputTokens
			t.Usage.CachedInputTokens += event.Usage.CachedInputTokens
			t.LastText = text
		})
	case agent.EventToolCallStarted:
		lastTool := event.ToolName
		if preview := compactArgsPreview(event.ToolArgs, 60); preview != "" {
			lastTool = event.ToolName + " " + preview
		}
		p.tasks.Update(p.taskID, func(t *agent.Task) {
			t.ToolCalls++
			t.LastTool = lastTool
		})
	}
}

// CompletionText renders the notification/wait text for a final task.
func CompletionText(task agent.Task, maxOutputBytes int) string {
	name := agentLabel(task)
	duration := completionDuration(task)
	calls := pluralizeToolCalls(task.ToolCalls)
	modelSegment := ""
	if task.Model != "" {
		modelSegment = " · " + task.Model
	}

	switch task.Status {
	case agent.TaskSucceeded:
		tokens := CommaInt(task.Usage.InputTokens + task.Usage.OutputTokens)
		header := fmt.Sprintf("[task-notification] task %s %s succeeded%s · %s · %s · %s tokens", task.ID, name, modelSegment, duration, calls, tokens)
		return header + "\n" + tool.CappedTextResult(task.Result, maxOutputBytes).Content
	case agent.TaskFailed:
		header := fmt.Sprintf("[task-notification] task %s %s failed%s · %s · %s", task.ID, name, modelSegment, duration, calls)
		return header + "\n" + task.Error
	default: // agent.TaskCanceled
		return fmt.Sprintf("[task-notification] task %s %s canceled%s · %s · %s", task.ID, name, modelSegment, duration, calls)
	}
}

// agentLabel is the parenthesized name shown after a task id, e.g.
// "(default)" or "(explorer)". Named definitions do not exist yet in this
// phase, so Task.Agent is always empty and every task shows "(default)".
func agentLabel(task agent.Task) string {
	name := task.Agent
	if name == "" {
		name = "default"
	}
	return "(" + name + ")"
}

// completionDuration is FinishedAt-StartedAt rounded to the second, or
// "0s" for a task canceled while still queued (StartedAt never set).
func completionDuration(task agent.Task) string {
	if task.StartedAt.IsZero() {
		return "0s"
	}
	return task.FinishedAt.Sub(task.StartedAt).Round(time.Second).String()
}

// lastAssistantText returns the trimmed text of the last assistant message
// in messages, or "" when there is none.
func lastAssistantText(messages []model.Message) string {
	for i := len(messages) - 1; i >= 0; i-- {
		if messages[i].Role == model.RoleAssistant {
			return strings.TrimSpace(messages[i].Text())
		}
	}
	return ""
}

func pluralizeToolCalls(n int) string {
	if n == 1 {
		return "1 tool call"
	}
	return fmt.Sprintf("%d tool calls", n)
}

func pluralizeTools(n int) string {
	if n == 1 {
		return "1 tool"
	}
	return fmt.Sprintf("%d tools", n)
}

// truncateRunesEllipsis returns s unchanged if it has at most maxRunes
// runes; otherwise its first maxRunes-1 runes plus "…", so the truncated
// result always has exactly maxRunes runes.
func truncateRunesEllipsis(s string, maxRunes int) string {
	runes := []rune(s)
	if len(runes) <= maxRunes {
		return s
	}
	if maxRunes <= 0 {
		return ""
	}
	return string(runes[:maxRunes-1]) + "…"
}

// capLastBytes returns the last maxBytes bytes of s, advanced forward to
// the next UTF-8 rune boundary so the result never starts mid-rune.
func capLastBytes(s string, maxBytes int) string {
	if len(s) <= maxBytes {
		return s
	}
	start := len(s) - maxBytes
	for start < len(s) && !utf8.RuneStart(s[start]) {
		start++
	}
	return s[start:]
}

// compactJSON removes insignificant whitespace from raw JSON. Invalid JSON
// falls back to whitespace-collapsing the raw text.
func compactJSON(raw string) string {
	var buf bytes.Buffer
	if json.Compact(&buf, []byte(raw)) == nil {
		return buf.String()
	}
	return OneLine(raw)
}

// compactArgsPreview renders tool call arguments as compact JSON capped at
// maxRunes runes (with a trailing "…" when truncated).
func compactArgsPreview(raw string, maxRunes int) string {
	return truncateRunesEllipsis(compactJSON(raw), maxRunes)
}
