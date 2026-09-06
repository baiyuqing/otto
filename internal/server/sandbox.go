package server

import (
	"net/http"

	"github.com/baiyuqing/otto/internal/app"
)

// handleSandboxReload re-reads the sandbox configuration and applies it to the
// running process. One sandbox serves every open session, so the reload is
// refused while any session has a turn in flight: replacing the executor while
// a bash command is using it would otherwise block until that command ends.
func (s *Server) handleSandboxReload(w http.ResponseWriter, r *http.Request) {
	reload := s.opts.ReloadSandbox
	if reload == nil {
		writeError(w, http.StatusNotImplemented, "not_implemented", "sandbox reload is not available")
		return
	}
	if s.anyTurnActive() {
		writeError(w, http.StatusConflict, "turn_active", "a turn is active; sandbox reload would replace a running command's sandbox")
		return
	}
	info, err := reload(r.Context())
	if err != nil {
		writeError(w, http.StatusConflict, "sandbox_reload_failed", err.Error())
		return
	}
	writeJSON(w, http.StatusOK, sandboxInfoWire(info))
}

func (s *Server) anyTurnActive() bool {
	s.mu.Lock()
	sessions := make([]*openSession, 0, len(s.sessions))
	for _, sess := range s.sessions {
		sessions = append(sessions, sess)
	}
	s.mu.Unlock()

	for _, sess := range sessions {
		sess.mu.Lock()
		t := sess.turn
		sess.mu.Unlock()
		if t != nil && t.summary().Status == turnRunning {
			return true
		}
	}
	return false
}

func sandboxInfoWire(info app.SandboxInfo) sandboxWire {
	return sandboxWire{
		Mode:          string(info.Mode),
		Network:       string(info.Network),
		BashAvailable: info.BashAvailable,
		Summary:       info.Summary(),
	}
}
