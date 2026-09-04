package subagent

import (
	"fmt"
	"io"
	"strconv"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
)

// TaskLine renders one task in the layout shared with agent_status: id,
// name, status, elapsed, tool count, last tool (or final token total), and a
// description/prompt label.
func TaskLine(task agent.Task, now time.Time) string {
	name := task.Agent
	if name == "" {
		name = "(default)"
	}
	line := fmt.Sprintf("%-4s %-10s %-9s %6s %9s  %-24s %s",
		task.ID, name, string(task.Status), TaskElapsed(task, now), TaskToolsColumn(task), TaskDetail(task), TaskLabel(task))
	return strings.TrimRight(line, " ")
}

// TaskElapsed renders a task's elapsed time: empty while queued, the
// running duration to now, or the finished duration. A task canceled while
// still queued never got a StartedAt; its elapsed time reads "0s" rather
// than measuring from the zero time.
func TaskElapsed(task agent.Task, now time.Time) string {
	switch task.Status {
	case agent.TaskQueued:
		return ""
	case agent.TaskRunning:
		return now.Sub(task.StartedAt).Round(time.Second).String()
	default:
		if task.StartedAt.IsZero() {
			return "0s"
		}
		return task.FinishedAt.Sub(task.StartedAt).Round(time.Second).String()
	}
}

// TaskToolsColumn renders a task's tool-call count, e.g. "1 tool" or
// "4 tools"; empty while queued.
func TaskToolsColumn(task agent.Task) string {
	if task.Status == agent.TaskQueued {
		return ""
	}
	if task.ToolCalls == 1 {
		return "1 tool"
	}
	return fmt.Sprintf("%d tools", task.ToolCalls)
}

// TaskDetail renders a task's current activity (its last tool) or, once
// finished, its total token count; empty while queued.
func TaskDetail(task agent.Task) string {
	if task.Status == agent.TaskQueued {
		return ""
	}
	if task.Final() {
		return fmt.Sprintf("%s tokens", CommaInt(task.Usage.InputTokens+task.Usage.OutputTokens))
	}
	return task.LastTool
}

// TaskLabel is a task's description, or the first 60 runes of its prompt
// collapsed to one line when no description was given, prefixed by the
// task's name and ": " when it has one.
func TaskLabel(task agent.Task) string {
	label := task.Description
	if label == "" {
		label = FirstRunes(OneLine(task.Prompt), 60)
	}
	if task.Name != "" {
		label = task.Name + ": " + label
	}
	return label
}

// OneLine collapses s to a single line, joining fields with a single space.
func OneLine(s string) string {
	return strings.Join(strings.Fields(s), " ")
}

// FirstRunes returns the first n runes of s, unchanged if s has n runes or
// fewer.
func FirstRunes(s string, n int) string {
	runes := []rune(s)
	if len(runes) <= n {
		return s
	}
	return string(runes[:n])
}

// CommaInt formats n with thousands separators, e.g. 12310 -> "12,310".
func CommaInt(n int) string {
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

// TaskSteps renders a child's transcript as tool calls and assistant text,
// in order. Tool results and the delegated prompt (a user message) are not
// shown, matching agent_status's step listing.
func TaskSteps(history []model.Message) string {
	var b strings.Builder
	writeTaskSteps(&b, history)
	return b.String()
}

func writeTaskSteps(w io.Writer, history []model.Message) {
	for _, message := range history {
		for _, block := range message.Blocks {
			switch {
			case block.Type == model.BlockToolCall:
				_, _ = fmt.Fprintf(w, "  → %s %s\n", block.ToolName, FirstRunes(OneLine(string(block.Arguments)), 80))
			case block.Type == model.BlockText && message.Role == model.RoleAssistant && block.Text != "":
				_, _ = fmt.Fprintf(w, "  assistant: %s\n", block.Text)
			}
		}
	}
}
