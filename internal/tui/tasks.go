package tui

import (
	"fmt"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/subagent"
	"github.com/charmbracelet/x/ansi"
)

// taskPanelMaxRows is how many task rows the live task panel shows before
// collapsing the rest into a single "+N more" line.
const taskPanelMaxRows = 5

// taskUpdateMsg reports a signal from the active task registry's Updates
// channel: a task or notification changed (closed is false), or the
// channel closed because a session replacement swapped in a new registry,
// or none is active any more (closed is true).
type taskUpdateMsg struct {
	closed bool
}

// waitTaskUpdate blocks on updates and reports whether the channel closed.
func waitTaskUpdate(updates <-chan struct{}) tea.Cmd {
	return func() tea.Msg {
		_, open := <-updates
		return taskUpdateMsg{closed: !open}
	}
}

// taskRegistry returns the backend's current sub-agent task registry, or
// nil when sub-agents are not available in this session.
func (m Model) taskRegistry() *agent.Tasks {
	lister, ok := m.backend.(app.TaskLister)
	if !ok {
		return nil
	}
	return lister.Tasks()
}

// armTaskUpdates returns a command that waits on the current task
// registry's Updates channel, or nil when no registry is active.
func (m Model) armTaskUpdates() tea.Cmd {
	tasks := m.taskRegistry()
	if tasks == nil {
		return nil
	}
	return waitTaskUpdate(tasks.Updates())
}

// countActiveTasks reports how many tasks are queued or running right now.
func (m Model) countActiveTasks() int {
	tasks := m.taskRegistry()
	if tasks == nil {
		return 0
	}
	count := 0
	for _, task := range tasks.List() {
		if task.Status == agent.TaskQueued || task.Status == agent.TaskRunning {
			count++
		}
	}
	return count
}

// appendTaskEntry appends a system or error entry produced by a /tasks or
// /task command and refreshes the transcript.
func (m *Model) appendTaskEntry(kind EntryKind, text string) {
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("task"), Kind: kind, Raw: text})
	m.rerenderAndRefreshViewportContent()
}

// noteCanceledTasks appends a system entry reporting that a session
// replacement canceled n queued/running tasks, and refreshes the
// transcript to show it. A no-op when n is 0.
func (m *Model) noteCanceledTasks(n int) {
	if n == 0 {
		return
	}
	m.appendTaskEntry(EntrySystem, fmt.Sprintf("canceled %d running tasks", n))
}

// handleTasksCommand lists every sub-agent task as one line per task via
// subagent.TaskLine, mirroring the REPL's /tasks command.
func (m Model) handleTasksCommand() (tea.Model, tea.Cmd) {
	m.clearEditor()
	tasks := m.taskRegistry()
	if tasks == nil {
		m.appendTaskEntry(EntrySystem, "sub-agents are not available")
		return m, nil
	}
	list := tasks.List()
	if len(list) == 0 {
		m.appendTaskEntry(EntrySystem, "no tasks in this session")
		return m, nil
	}
	now := m.now()
	lines := make([]string, 0, len(list))
	for _, task := range list {
		lines = append(lines, subagent.TaskLine(task, now))
	}
	m.appendTaskEntry(EntrySystem, strings.Join(lines, "\n"))
	return m, nil
}

// handleTaskCommand shows one task's detail ("/task <id>") or cancels a
// queued/running task ("/task cancel <id>"), mirroring the REPL's /task
// command.
func (m Model) handleTaskCommand(argument string) (tea.Model, tea.Cmd) {
	m.clearEditor()
	tasks := m.taskRegistry()
	if tasks == nil {
		m.appendTaskEntry(EntrySystem, "sub-agents are not available")
		return m, nil
	}
	fields := strings.Fields(argument)
	if len(fields) == 2 && fields[0] == "cancel" {
		id := fields[1]
		if err := tasks.Cancel(id); err != nil {
			m.appendTaskEntry(EntryError, err.Error())
			return m, nil
		}
		m.appendTaskEntry(EntrySystem, "canceled "+id)
		return m, nil
	}
	if len(fields) != 1 || fields[0] == "cancel" {
		m.appendTaskEntry(EntrySystem, "usage: /task <id|name> | /task cancel <id|name>")
		return m, nil
	}
	id := fields[0]
	task, found := tasks.Get(id)
	if !found {
		m.appendTaskEntry(EntryError, "unknown task: "+id)
		return m, nil
	}
	var b strings.Builder
	b.WriteString(subagent.TaskLine(task, m.now()))
	b.WriteString("\n")
	if task.Model != "" {
		fmt.Fprintf(&b, "model: %s\n", task.Model)
	}
	if history, _ := tasks.History(id); len(history) > 0 {
		b.WriteString(subagent.TaskSteps(history))
	}
	if task.Final() {
		if task.Error != "" {
			fmt.Fprintf(&b, "error: %s\n", task.Error)
		} else {
			fmt.Fprintf(&b, "result: %s\n", task.Result)
		}
	}
	m.appendTaskEntry(EntrySystem, strings.TrimRight(b.String(), "\n"))
	return m, nil
}

// activeTasksFromRegistry returns the queued/running tasks in a registry, in
// creation order. A pure function of the registry so the panel-rendering
// tests below do not need a Model.
func activeTasksFromRegistry(tasks *agent.Tasks) []agent.Task {
	if tasks == nil {
		return nil
	}
	var active []agent.Task
	for _, task := range tasks.List() {
		if task.Status == agent.TaskQueued || task.Status == agent.TaskRunning {
			active = append(active, task)
		}
	}
	return active
}

// activeTasks returns the backend's currently queued/running tasks.
func (m Model) activeTasks() []agent.Task {
	return activeTasksFromRegistry(m.taskRegistry())
}

// taskPanelLines reports how many lines the task panel currently occupies.
func (m Model) taskPanelLines() int {
	return taskPanelLineCount(m.activeTasks())
}

// taskPanelLineCount reports how many lines the task panel occupies: 0 when
// no task is queued or running, otherwise a header line plus up to
// taskPanelMaxRows task rows, plus one "+N more" line when more tasks are
// active than fit.
func taskPanelLineCount(active []agent.Task) int {
	if len(active) == 0 {
		return 0
	}
	lines := 1 + min(len(active), taskPanelMaxRows)
	if len(active) > taskPanelMaxRows {
		lines++
	}
	return lines
}

// taskPanelContent renders the task panel: a header summarizing running and
// queued counts, up to taskPanelMaxRows task rows, and a final "+N more"
// line when more tasks are active than fit. Empty when active is empty.
func taskPanelContent(active []agent.Task, now time.Time, spinnerFrame string, width int) string {
	if len(active) == 0 {
		return ""
	}
	lines := []string{taskPanelHeader(active)}
	shown := active
	if len(shown) > taskPanelMaxRows {
		shown = shown[:taskPanelMaxRows]
	}
	for _, task := range shown {
		lines = append(lines, taskPanelRow(task, now, spinnerFrame, width))
	}
	if more := len(active) - len(shown); more > 0 {
		lines = append(lines, fmt.Sprintf("+%d more", more))
	}
	return strings.Join(lines, "\n")
}

// taskPanelHeader summarizes the active tasks as counts by status, e.g.
// "tasks  2 running · 1 queued".
func taskPanelHeader(active []agent.Task) string {
	running, queued := 0, 0
	for _, task := range active {
		if task.Status == agent.TaskRunning {
			running++
		} else {
			queued++
		}
	}
	var parts []string
	if running > 0 {
		parts = append(parts, fmt.Sprintf("%d running", running))
	}
	if queued > 0 {
		parts = append(parts, fmt.Sprintf("%d queued", queued))
	}
	return "tasks  " + strings.Join(parts, " · ")
}

// taskPanelRow renders one task row: a status marker (the spinner frame
// while running, "·" while queued), id, agent name, elapsed time, tool
// count, current activity, and a description/prompt label. The label
// column is dropped first when width is too small to fit it.
func taskPanelRow(task agent.Task, now time.Time, spinnerFrame string, width int) string {
	marker := "·"
	if task.Status == agent.TaskRunning {
		marker = spinnerFrame
	}
	name := task.Agent
	if name == "" {
		name = "(default)"
	}
	elapsed := "queued"
	if task.Status != agent.TaskQueued {
		elapsed = subagent.TaskElapsed(task, now)
	}
	tools := subagent.TaskToolsColumn(task)
	detail := subagent.FirstRunes(subagent.TaskDetail(task), 24)
	label := subagent.TaskLabel(task)

	prefix := strings.TrimRight(fmt.Sprintf(" %s %-4s %-10s %-7s %-8s %-24s", marker, task.ID, name, elapsed, tools, detail), " ")
	if width <= 0 {
		return prefix
	}
	row := prefix
	if available := width - ansi.StringWidth(prefix) - 2; available > 0 {
		row = prefix + "  " + subagent.FirstRunes(label, available)
	}
	if ansi.StringWidth(row) > width {
		row = ansi.Truncate(row, width, "")
	}
	return row
}

// wakeBlockedByReservedState reports whether a wake turn must not start: a
// turn is already running, a session-replacement or other async command
// (/new, /resume, /model, /login) is in flight, or a modal, overlay, or
// picker is open.
func (m Model) wakeBlockedByReservedState() bool {
	return m.running || m.newSessionPending || m.profileSwitchPending || m.loginPending ||
		m.resume.active() || m.resume.listPending || m.archive.active() || m.archive.listPending ||
		m.profilePicker.active() || m.overlay != overlayNone
}

// maybeWake starts an empty-text turn to drain the notification inbox when
// a sub-agent task has a pending notification, no turn is active, and no
// session-replacement command or modal is in flight. It reuses the same
// turn-state setup as a user-submitted prompt, without appending an
// EntryUser to the transcript.
func (m Model) maybeWake() (Model, tea.Cmd) {
	tasks := m.taskRegistry()
	if tasks == nil || tasks.Pending() == 0 {
		return m, nil
	}
	if m.wakeBlockedByReservedState() {
		return m, nil
	}
	return m.startTurn("", false)
}
