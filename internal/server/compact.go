package server

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
)

// handleCompact runs one context compaction on the session and returns the
// shared compaction payload. It is refused with 409 turn_active while a turn
// or another compaction is running; startTurn refuses turns for the same
// window. The compaction is canceled when the client disconnects.
func (s *Server) handleCompact(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}

	var body struct {
		Focus string `json:"focus"`
	}
	if r.ContentLength != 0 {
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil && !errors.Is(err, io.EOF) {
			writeError(w, http.StatusBadRequest, "bad_request", "invalid JSON body")
			return
		}
	}

	os.mu.Lock()
	if (os.turn != nil && !os.turn.isDone()) || os.compactCancel != nil {
		os.mu.Unlock()
		writeError(w, http.StatusConflict, "turn_active", "a turn is already active for this session")
		return
	}
	ctx, cancel := context.WithCancel(s.ctx)
	os.compactCancel = cancel
	os.mu.Unlock()
	stop := context.AfterFunc(r.Context(), cancel)
	defer func() {
		stop()
		cancel()
		os.mu.Lock()
		os.compactCancel = nil
		os.mu.Unlock()
	}()

	result, err := os.ctrl.Compact(ctx, body.Focus, func(agent.Event) {})
	switch {
	case err == nil:
		s.log.Info("compaction_finished", "session_id", r.PathValue("id"), "noop", result.Noop, "tokens_before", result.TokensBefore)
		writeJSON(w, http.StatusOK, toWireCompaction(&result))
	case errors.Is(err, app.ErrPromptActive):
		writeError(w, http.StatusConflict, "turn_active", "a turn is already active for this session")
	case errors.Is(err, context.Canceled):
		// Client gone or server shutting down; nothing to write.
	default:
		s.log.Error("compaction_error", "session_id", r.PathValue("id"), "error", err)
		writeError(w, http.StatusConflict, "compaction_failed", err.Error())
	}
}
