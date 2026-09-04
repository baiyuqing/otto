package repl

import (
	"context"
	"fmt"
	"io"
	"strconv"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

// taskLister returns the active runner's task registry and whether it is
// available: the backend must implement app.TaskLister and its Tasks() must
// be non-nil (absent when sub-agents are disabled or no runner is active).
func (r *REPL) taskLister() (*agent.Tasks, bool) {
	lister, ok := r.backend.(app.TaskLister)
	if !ok {
		return nil, false
	}
	tasks := lister.Tasks()
	return tasks, tasks != nil
}

func (r *REPL) tasksCommand(ctx context.Context) (bool, error) {
	tasks, ok := r.taskLister()
	if !ok {
		_, _ = fmt.Fprintln(r.stderr, "sub-agents are not available")
		return false, nil
	}
	list := tasks.List()
	if len(list) == 0 {
		_, _ = fmt.Fprintln(r.stdout, "no tasks in this session")
		return false, nil
	}
	now := time.Now()
	for _, task := range list {
		_, _ = fmt.Fprintln(r.stdout, taskLine(task, now))
	}
	return false, nil
}

func (r *REPL) taskCommand(ctx context.Context, args string) (bool, error) {
	tasks, ok := r.taskLister()
	if !ok {
		_, _ = fmt.Fprintln(r.stderr, "sub-agents are not available")
		return false, nil
	}
	fields := strings.Fields(args)
	if len(fields) == 2 && fields[0] == "cancel" {
		id := fields[1]
		if err := tasks.Cancel(id); err != nil {
			_, _ = fmt.Fprintln(r.stderr, err)
			return false, nil
		}
		_, _ = fmt.Fprintf(r.stdout, "canceled %s\n", id)
		return false, nil
	}
	if len(fields) != 1 || fields[0] == "cancel" {
		_, _ = fmt.Fprintln(r.stderr, "usage: /task <id> | /task cancel <id>")
		return false, nil
	}
	id := fields[0]
	task, found := tasks.Get(id)
	if !found {
		_, _ = fmt.Fprintf(r.stderr, "unknown task: %s\n", id)
		return false, nil
	}
	_, _ = fmt.Fprintln(r.stdout, taskLine(task, time.Now()))
	if task.Model != "" {
		_, _ = fmt.Fprintf(r.stdout, "model: %s\n", task.Model)
	}
	if history, _ := tasks.History(id); len(history) > 0 {
		writeTaskSteps(r.stdout, history)
	}
	if task.Final() {
		if task.Error != "" {
			_, _ = fmt.Fprintf(r.stdout, "error: %s\n", task.Error)
		} else {
			_, _ = fmt.Fprintf(r.stdout, "result: %s\n", task.Result)
		}
	}
	return false, nil
}

// writeTaskSteps renders a child's transcript as tool calls and assistant
// text, in order. Tool results and the delegated prompt (a user message) are
// not shown, matching agent_status's step listing.
func writeTaskSteps(w io.Writer, history []model.Message) {
	for _, message := range history {
		for _, block := range message.Blocks {
			switch {
			case block.Type == model.BlockToolCall:
				_, _ = fmt.Fprintf(w, "  → %s %s\n", block.ToolName, firstRunes(oneLine(string(block.Arguments)), 80))
			case block.Type == model.BlockText && message.Role == model.RoleAssistant && block.Text != "":
				_, _ = fmt.Fprintf(w, "  assistant: %s\n", block.Text)
			}
		}
	}
}

// taskLine renders one task in the layout shared with agent_status: id,
// name, status, elapsed, tool count, last tool (or final token total), and a
// description/prompt label.
func taskLine(task agent.Task, now time.Time) string {
	name := task.Agent
	if name == "" {
		name = "(default)"
	}
	line := fmt.Sprintf("%-4s %-10s %-9s %6s %9s  %-24s %s",
		task.ID, name, string(task.Status), taskElapsed(task, now), taskToolsColumn(task), taskDetail(task), taskLabel(task))
	return strings.TrimRight(line, " ")
}

func taskElapsed(task agent.Task, now time.Time) string {
	switch task.Status {
	case agent.TaskQueued:
		return ""
	case agent.TaskRunning:
		return now.Sub(task.StartedAt).Round(time.Second).String()
	default:
		// A task canceled while still queued never got a StartedAt; treat
		// its elapsed time as 0s rather than measuring from the zero time.
		if task.StartedAt.IsZero() {
			return "0s"
		}
		return task.FinishedAt.Sub(task.StartedAt).Round(time.Second).String()
	}
}

func taskToolsColumn(task agent.Task) string {
	if task.Status == agent.TaskQueued {
		return ""
	}
	if task.ToolCalls == 1 {
		return "1 tool"
	}
	return fmt.Sprintf("%d tools", task.ToolCalls)
}

func taskDetail(task agent.Task) string {
	if task.Status == agent.TaskQueued {
		return ""
	}
	if task.Final() {
		return fmt.Sprintf("%s tokens", commaInt(task.Usage.InputTokens+task.Usage.OutputTokens))
	}
	return task.LastTool
}

func taskLabel(task agent.Task) string {
	if task.Description != "" {
		return task.Description
	}
	return firstRunes(oneLine(task.Prompt), 60)
}

func oneLine(s string) string {
	return strings.Join(strings.Fields(s), " ")
}

func firstRunes(s string, n int) string {
	runes := []rune(s)
	if len(runes) <= n {
		return s
	}
	return string(runes[:n])
}

// commaInt formats n with thousands separators, e.g. 12310 -> "12,310".
func commaInt(n int) string {
	s := strconv.Itoa(n)
	neg := strings.HasPrefix(s, "-")
	if neg {
		s = s[1:]
	}
	for i := len(s) - 3; i > 0; i -= 3 {
		s = s[:i] + "," + s[i:]
	}
	if neg {
		s = "-" + s
	}
	return s
}
