package subagent

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/tool"
)

const (
	defaultWaitTimeoutSeconds = 600
	maxWaitTimeoutSeconds     = 3600
)

const agentDescription = "Start a sub-agent on a self-contained task and return immediately with its task id. The sub-agent runs in parallel with you, has its own context, the same workspace and file tools, and cannot see this conversation. Its final report arrives later as a [task-notification] message. Use agent_wait when you need the result before continuing, agent_status to check progress. Put everything the sub-agent needs into prompt: goal, relevant paths, what to report back."

const agentWaitDescription = "Wait for a sub-agent task to finish. With task_id, waits for that task; without it, waits for every task that is queued or running. Blocks up to timeout_seconds (default 600, max 3600) and returns each task's completion report. Errors if the wait times out or is canceled, naming the tasks still running, or if task_id is unknown."

const agentStatusDescription = "Show sub-agent task status. Without task_id, one line per task in this session: id, status, elapsed time, and current activity or token total. With task_id, that line plus the task's recent steps and, once finished, its result or error."

// Tools returns the parent-side tools: agent, agent_wait, agent_status, in
// that order.
func (r *Runner) Tools() []tool.Tool {
	return []tool.Tool{
		&agentTool{runner: r},
		&agentWaitTool{runner: r},
		&agentStatusTool{runner: r},
	}
}

// ToolDefinitions returns the agent, agent_wait, and agent_status tool
// definitions without a fully built Runner. It lets callers (the redaction
// boundary check in cmd/otto) predict the tool set a real Runner would
// register without duplicating these JSON schemas outside this package;
// none of the three Definition() methods read Runner fields.
func ToolDefinitions() []model.ToolDefinition {
	zero := &Runner{}
	return []model.ToolDefinition{
		(&agentTool{runner: zero}).Definition(),
		(&agentWaitTool{runner: zero}).Definition(),
		(&agentStatusTool{runner: zero}).Definition(),
	}
}

type agentTool struct {
	runner *Runner
}

type agentToolArgs struct {
	Prompt      string `json:"prompt"`
	Description string `json:"description,omitempty"`
	Wait        bool   `json:"wait,omitempty"`
	Model       string `json:"model,omitempty"`
}

func (t *agentTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "agent",
		Description: agentDescription,
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"prompt": map[string]any{
					"type":        "string",
					"description": "The complete task; the sub-agent sees nothing else.",
				},
				"description": map[string]any{
					"type":        "string",
					"description": "Short label (<= 80 chars) shown in status output.",
				},
				"wait": map[string]any{
					"type":        "boolean",
					"description": "Block until the task ends and return its result instead of its id.",
				},
				"model": map[string]any{
					"type":        "string",
					"description": "Provider model id for the sub-agent. Default: this session's model. Otto does not validate it; an id the endpoint rejects fails the task with the provider's error.",
				},
			},
			"required": []string{"prompt"},
		},
	}
}

func (t *agentTool) Execute(ctx context.Context, arguments json.RawMessage) tool.Result {
	var args agentToolArgs
	if err := tool.DecodeStrictJSON(arguments, &args, "prompt"); err != nil {
		return tool.Result{Content: err.Error(), IsError: true}
	}
	if strings.TrimSpace(args.Prompt) == "" {
		return tool.Result{Content: "prompt is required", IsError: true}
	}

	runningBefore := countRunning(t.runner.config.Tasks)
	task, err := t.runner.Start(StartRequest{Prompt: args.Prompt, Description: args.Description, Model: args.Model})
	if err != nil {
		return tool.Result{Content: err.Error(), IsError: true}
	}

	name := agentLabel(task)
	started := fmt.Sprintf("task %s %s started", task.ID, name)
	if runningBefore >= t.runner.config.MaxParallel {
		started = fmt.Sprintf("task %s %s queued (%d running, limit %d)", task.ID, name, runningBefore, t.runner.config.MaxParallel)
	}

	if !args.Wait {
		return tool.Result{Content: started}
	}

	text, remaining, waitErr := waitTasks(ctx, t.runner.config.Tasks, []string{task.ID}, t.runner.config.MaxOutputBytes)
	if waitErr != nil {
		return tool.Result{Content: fmt.Sprintf("wait canceled; still running: %s", strings.Join(remaining, ", ")), IsError: true}
	}
	return tool.Result{Content: text}
}

func countRunning(tasks *agent.Tasks) int {
	n := 0
	for _, t := range tasks.List() {
		if t.Status == agent.TaskRunning {
			n++
		}
	}
	return n
}

// waitTasks blocks on each task in order via tasks.Wait(waitCtx, id),
// stopping at the first that does not complete. On success it removes each
// task's finished notification so it is not delivered twice by the inbox,
// and returns their completion texts joined with a blank line. On failure
// it returns the ids from the failing one onward and the wait error,
// without removing any notification or returning any text.
func waitTasks(waitCtx context.Context, tasks *agent.Tasks, ids []string, maxOutputBytes int) (text string, remaining []string, err error) {
	completed := make([]agent.Task, 0, len(ids))
	for i, id := range ids {
		task, waitErr := tasks.Wait(waitCtx, id)
		if waitErr != nil {
			return "", ids[i:], waitErr
		}
		completed = append(completed, task)
	}
	texts := make([]string, len(completed))
	for i, task := range completed {
		tasks.Notifications().Remove(task.ID, agent.NotificationTaskFinished)
		texts[i] = CompletionText(task, maxOutputBytes)
	}
	return strings.Join(texts, "\n\n"), nil, nil
}

type agentWaitTool struct {
	runner *Runner
}

type agentWaitToolArgs struct {
	TaskID         string `json:"task_id,omitempty"`
	TimeoutSeconds int    `json:"timeout_seconds,omitempty"`
}

func (t *agentWaitTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "agent_wait",
		Description: agentWaitDescription,
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"task_id": map[string]any{
					"type":        "string",
					"description": "Task to wait for; omit to wait for every queued or running task.",
				},
				"timeout_seconds": map[string]any{
					"type":        "integer",
					"description": "Maximum time to wait, in seconds. Default 600, max 3600.",
				},
			},
		},
	}
}

func (t *agentWaitTool) Execute(ctx context.Context, arguments json.RawMessage) tool.Result {
	var args agentWaitToolArgs
	if err := tool.DecodeStrictJSON(arguments, &args); err != nil {
		return tool.Result{Content: err.Error(), IsError: true}
	}
	if args.TimeoutSeconds < 0 {
		return tool.Result{Content: "timeout_seconds must not be negative", IsError: true}
	}
	timeoutSeconds := args.TimeoutSeconds
	if timeoutSeconds == 0 {
		timeoutSeconds = defaultWaitTimeoutSeconds
	}
	if timeoutSeconds > maxWaitTimeoutSeconds {
		timeoutSeconds = maxWaitTimeoutSeconds
	}
	timeout := time.Duration(timeoutSeconds) * time.Second

	tasks := t.runner.config.Tasks
	var ids []string
	if args.TaskID != "" {
		if _, ok := tasks.Get(args.TaskID); !ok {
			return tool.Result{Content: fmt.Sprintf("unknown task: %s", args.TaskID), IsError: true}
		}
		ids = []string{args.TaskID}
	} else {
		for _, task := range tasks.List() {
			if !task.Final() {
				ids = append(ids, task.ID)
			}
		}
		if len(ids) == 0 {
			return tool.Result{Content: "no tasks are running"}
		}
	}

	waitCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	text, remaining, waitErr := waitTasks(waitCtx, tasks, ids, t.runner.config.MaxOutputBytes)
	if waitErr != nil {
		if ctx.Err() != nil {
			return tool.Result{Content: fmt.Sprintf("wait canceled; still running: %s", strings.Join(remaining, ", ")), IsError: true}
		}
		return tool.Result{Content: fmt.Sprintf("timed out after %ds; still running: %s", timeoutSeconds, strings.Join(remaining, ", ")), IsError: true}
	}
	return tool.Result{Content: text}
}

type agentStatusTool struct {
	runner *Runner
}

type agentStatusToolArgs struct {
	TaskID string `json:"task_id,omitempty"`
}

func (t *agentStatusTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "agent_status",
		Description: agentStatusDescription,
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"task_id": map[string]any{
					"type":        "string",
					"description": "Show one task's detail, including its recent steps.",
				},
			},
		},
	}
}

func (t *agentStatusTool) Execute(ctx context.Context, arguments json.RawMessage) tool.Result {
	var args agentStatusToolArgs
	if err := tool.DecodeStrictJSON(arguments, &args); err != nil {
		return tool.Result{Content: err.Error(), IsError: true}
	}

	tasks := t.runner.config.Tasks
	all := tasks.List()
	if len(all) == 0 {
		return tool.Result{Content: "no tasks in this session"}
	}

	if args.TaskID == "" {
		now := t.runner.now()
		lines := make([]string, 0, len(all))
		for _, task := range all {
			lines = append(lines, statusLine(task, now))
		}
		return tool.Result{Content: strings.Join(lines, "\n")}
	}

	task, ok := tasks.Get(args.TaskID)
	if !ok {
		return tool.Result{Content: fmt.Sprintf("unknown task: %s", args.TaskID), IsError: true}
	}
	lines := []string{statusLine(task, t.runner.now())}
	if task.Model != "" {
		lines = append(lines, "model: "+task.Model)
	}
	history, _ := tasks.History(args.TaskID)
	lines = append(lines, historyLines(history)...)
	if task.Final() {
		if task.Error != "" {
			lines = append(lines, "error: "+task.Error)
		} else {
			lines = append(lines, "result:", task.Result)
		}
	}
	return tool.Result{Content: strings.Join(lines, "\n")}
}

// statusLine renders one agent_status table row: fixed-width id, name,
// status, elapsed, tool count, detail, and label columns, trailing spaces
// trimmed.
func statusLine(task agent.Task, now time.Time) string {
	elapsed := ""
	switch {
	case task.Status == agent.TaskRunning:
		elapsed = now.Sub(task.StartedAt).Round(time.Second).String()
	case task.Final() && !task.StartedAt.IsZero():
		elapsed = task.FinishedAt.Sub(task.StartedAt).Round(time.Second).String()
	}

	toolsColumn := ""
	if task.ToolCalls > 0 {
		toolsColumn = pluralizeTools(task.ToolCalls)
	}

	detail := task.LastTool
	if task.Final() {
		detail = formatThousands(task.Usage.InputTokens+task.Usage.OutputTokens) + " tokens"
	}

	label := task.Description
	if label == "" {
		label = collapseToOneLine(truncateRunes(task.Prompt, 60))
	}

	line := fmt.Sprintf("%-4s %-10s %-9s %6s %9s  %-24s %s", task.ID, agentLabel(task), string(task.Status), elapsed, toolsColumn, detail, label)
	return strings.TrimRight(line, " ")
}

// historyLines renders each assistant step's tool calls and text as lines,
// keeping only the last 10 across the whole history.
func historyLines(history []model.Message) []string {
	var lines []string
	for _, msg := range history {
		if msg.Role != model.RoleAssistant {
			continue
		}
		for _, block := range msg.Blocks {
			if block.Type != model.BlockToolCall {
				continue
			}
			line := "  → " + block.ToolName
			if preview := truncateRunes(compactJSON(string(block.Arguments)), 80); preview != "" {
				line += " " + preview
			}
			lines = append(lines, line)
		}
		if text := strings.TrimSpace(msg.Text()); text != "" {
			lines = append(lines, "  assistant: "+capLastBytes(text, 500))
		}
	}
	if len(lines) > 10 {
		lines = lines[len(lines)-10:]
	}
	return lines
}
