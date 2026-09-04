package repl

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/subagent"
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
		_, _ = fmt.Fprintln(r.stdout, subagent.TaskLine(task, now))
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
	_, _ = fmt.Fprintln(r.stdout, subagent.TaskLine(task, time.Now()))
	if task.Model != "" {
		_, _ = fmt.Fprintf(r.stdout, "model: %s\n", task.Model)
	}
	if history, _ := tasks.History(id); len(history) > 0 {
		_, _ = fmt.Fprint(r.stdout, subagent.TaskSteps(history))
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
