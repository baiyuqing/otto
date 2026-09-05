package server

import (
	"errors"
	"net/http"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
)

// taskWire is the wire form of agent.Task, per
// docs/specs/2026-09-03-agent-server-design.md "Sub-agent tasks".
type taskWire struct {
	ID           string      `json:"id"`
	Name         string      `json:"name,omitempty"`
	Agent        string      `json:"agent"`
	Description  string      `json:"description"`
	Model        string      `json:"model,omitempty"`
	Status       string      `json:"status"`
	CreatedAt    time.Time   `json:"created_at"`
	StartedAt    *time.Time  `json:"started_at,omitempty"`
	FinishedAt   *time.Time  `json:"finished_at,omitempty"`
	Steps        int         `json:"steps"`
	ToolCalls    int         `json:"tool_calls"`
	LastTool     string      `json:"last_tool,omitempty"`
	LastText     string      `json:"last_text,omitempty"`
	Usage        model.Usage `json:"usage"`
	UsagePresent bool        `json:"usage_present"`
	Result       string      `json:"result,omitempty"`
	Error        string      `json:"error,omitempty"`
}

// optionalTime returns nil for a zero time.Time, else a pointer to it, so
// wire fields serialize to null/omitted rather than the zero-value
// timestamp.
func optionalTime(t time.Time) *time.Time {
	if t.IsZero() {
		return nil
	}
	return &t
}

func toTaskWire(task agent.Task) taskWire {
	return taskWire{
		ID:           task.ID,
		Name:         task.Name,
		Agent:        task.Agent,
		Description:  task.Description,
		Model:        task.Model,
		Status:       string(task.Status),
		CreatedAt:    task.CreatedAt,
		StartedAt:    optionalTime(task.StartedAt),
		FinishedAt:   optionalTime(task.FinishedAt),
		Steps:        task.Steps,
		ToolCalls:    task.ToolCalls,
		LastTool:     task.LastTool,
		LastText:     task.LastText,
		Usage:        task.Usage,
		UsagePresent: task.UsagePresent,
		Result:       task.Result,
		Error:        task.Error,
	}
}

func (s *Server) handleListTasks(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	tasks := os.ctrl.Tasks()
	if tasks == nil {
		writeJSON(w, http.StatusOK, struct {
			Tasks []taskWire `json:"tasks"`
		}{Tasks: []taskWire{}})
		return
	}
	list := tasks.List()
	wire := make([]taskWire, len(list))
	for i, task := range list {
		wire[i] = toTaskWire(task)
	}
	writeJSON(w, http.StatusOK, struct {
		Tasks []taskWire `json:"tasks"`
	}{Tasks: wire})
}

func (s *Server) handleGetTask(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	taskID := r.PathValue("task_id")
	tasks := os.ctrl.Tasks()
	if tasks == nil {
		writeError(w, http.StatusNotFound, "not_found", "task not found")
		return
	}
	task, ok := tasks.Get(taskID)
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "task not found")
		return
	}
	history, _ := tasks.History(taskID)
	if history == nil {
		history = []model.Message{} // JSON "[]", not "null"
	}
	writeJSON(w, http.StatusOK, struct {
		taskWire
		History []model.Message `json:"history"`
	}{taskWire: toTaskWire(task), History: history})
}

func (s *Server) handleCancelTask(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	taskID := r.PathValue("task_id")
	tasks := os.ctrl.Tasks()
	if tasks == nil {
		writeError(w, http.StatusNotFound, "not_found", "task not found")
		return
	}
	if err := tasks.Cancel(taskID); err != nil {
		if errors.Is(err, agent.ErrTaskFinished) {
			writeError(w, http.StatusConflict, "task_done", "task already finished")
			return
		}
		writeError(w, http.StatusNotFound, "not_found", "task not found")
		return
	}
	task, _ := tasks.Get(taskID)
	writeJSON(w, http.StatusOK, toTaskWire(task))
}
