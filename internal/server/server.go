// Package server implements otto serve's HTTP+JSON+SSE frontend: one
// app.Controller per session, a turn event buffer decoupled from the
// agent's synchronous emit callback, Prometheus metrics, and slog request
// logging. See docs/specs/2026-09-03-agent-server-design.md.
package server

import (
	"context"
	"crypto/rand"
	"embed"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

// ErrSessionNotFound is returned by Options.Open when the requested session
// id has no corresponding session file.
var ErrSessionNotFound = errors.New("session not found")

// Info is process-level static info, unrelated to any particular session:
// the workspace, provider, profile, model, sandbox summary, and the
// available configuration profiles. Served at GET /v1/info.
type Info struct {
	Workspace string   `json:"workspace"`
	Provider  string   `json:"provider"`
	Profile   string   `json:"profile"`
	Model     string   `json:"model"`
	Sandbox   string   `json:"sandbox"`
	Profiles  []string `json:"profiles"`
}

// Options configures a Server.
type Options struct {
	Create func(ctx context.Context) (*app.Controller, error)
	Open   func(ctx context.Context, id string) (*app.Controller, error)
	List   func(ctx context.Context) (session.ListResult, error)
	Info   Info
	Logger *slog.Logger // nil -> TextHandler to stderr
}

// openSession is one entry in the session registry: an open controller and
// its most recent turn, if any.
type openSession struct {
	ctrl *app.Controller

	mu   sync.Mutex
	turn *turn

	// turnFinished is signaled (non-blocking, capacity 1) after every turn
	// on this session finishes. startWakeLoop is the only reader; routing
	// the end-of-turn wake check through it, instead of calling startTurn
	// directly from the finishing turn's own goroutine, serializes every
	// wake admission through one goroutine so stale triggers cannot race into
	// starting two turns for the same pending notification.
	turnFinished chan struct{}
}

func newOpenSession(ctrl *app.Controller) *openSession {
	return &openSession{ctrl: ctrl, turnFinished: make(chan struct{}, 1)}
}

// Server is otto serve's HTTP handler plus the session registry and turn
// lifecycle behind it. Construct with New.
type Server struct {
	ctx     context.Context // parent of every turn and every factory call
	opts    Options
	log     *slog.Logger
	metrics *metrics

	mu       sync.Mutex
	sessions map[string]*openSession
	opening  map[string]*openPlaceholder // in-flight Open calls, keyed by id

	wakeWG sync.WaitGroup // one entry per running wake loop (startWakeLoop)

	mux http.Handler
}

// openPlaceholder lets concurrent resumes of the same session id wait for
// one in-flight Open call instead of starting their own.
type openPlaceholder struct {
	ready chan struct{}
	sess  *openSession
	err   error
}

// New constructs a Server. ctx is the parent context for every turn and
// every session-factory call; it outlives any single HTTP request.
func New(ctx context.Context, opts Options) *Server {
	log := opts.Logger
	if log == nil {
		log = slog.New(slog.NewTextHandler(os.Stderr, nil))
	}
	s := &Server{
		ctx:      ctx,
		opts:     opts,
		log:      log,
		metrics:  newMetrics(),
		sessions: make(map[string]*openSession),
		opening:  make(map[string]*openPlaceholder),
	}
	s.mux = s.buildMux()
	return s
}

// Handler returns the Server's http.Handler.
func (s *Server) Handler() http.Handler { return s.mux }

// Close cancels every in-flight turn, then closes every open Controller.
// Canceling first ensures Close does not block on a turn finishing on its
// own (app.Controller.Close waits for any active Prompt to return).
func (s *Server) Close() error {
	s.mu.Lock()
	sessions := make([]*openSession, 0, len(s.sessions))
	for _, os := range s.sessions {
		sessions = append(sessions, os)
	}
	s.sessions = make(map[string]*openSession)
	s.mu.Unlock()

	for _, os := range sessions {
		os.mu.Lock()
		if os.turn != nil {
			os.turn.cancel()
		}
		os.mu.Unlock()
	}

	var errs []error
	for _, os := range sessions {
		if err := os.ctrl.Close(); err != nil {
			errs = append(errs, err)
		}
	}
	s.metrics.sessionsOpen(-len(sessions))
	s.wakeWG.Wait() // closing ctrl closes its Tasks registry, ending every wake loop
	return errors.Join(errs...)
}

// Serve runs s on l until ctx is canceled, then shuts down gracefully (5
// second grace period) and returns nil. It never returns http.ErrServerClosed.
func Serve(ctx context.Context, l net.Listener, s *Server) error {
	httpServer := &http.Server{
		Handler:           s.Handler(),
		ReadHeaderTimeout: 10 * time.Second,
	}

	errCh := make(chan error, 1)
	go func() { errCh <- httpServer.Serve(l) }()

	select {
	case <-ctx.Done():
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := httpServer.Shutdown(shutdownCtx); err != nil {
			_ = httpServer.Close()
		}
		<-errCh
		return nil
	case err := <-errCh:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	}
}

// newID mirrors cmd/otto's randomID (crypto/rand, 16 bytes, hex): kept as a
// duplicate because internal/server cannot import cmd/otto.
func newID() (string, error) {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return hex.EncodeToString(b), nil
}

//go:embed openapi.yaml
var openapiFS embed.FS

var openapiYAML = func() []byte {
	b, err := openapiFS.ReadFile("openapi.yaml")
	if err != nil {
		panic(err) // embedded at build time; a read failure is a build bug
	}
	return b
}()

// ---- routing ----

type routeEntry struct {
	pattern string // e.g. "POST /v1/sessions/{id}/turns"
	handler http.HandlerFunc
}

func (s *Server) routeTable() []routeEntry {
	return []routeEntry{
		{"POST /v1/sessions", s.handleCreateSession},
		{"GET /v1/sessions", s.handleListSessions},
		{"GET /v1/sessions/{id}", s.handleGetSession},
		{"DELETE /v1/sessions/{id}", s.handleDeleteSession},
		{"GET /v1/sessions/{id}/history", s.handleHistory},
		{"POST /v1/sessions/{id}/turns", s.handleStartTurn},
		{"GET /v1/sessions/{id}/turns/{turn_id}", s.handleGetTurn},
		{"GET /v1/sessions/{id}/turns/{turn_id}/events", s.handleTurnEvents},
		{"POST /v1/sessions/{id}/turns/{turn_id}/cancel", s.handleCancelTurn},
		{"GET /v1/sessions/{id}/tasks", s.handleListTasks},
		{"GET /v1/sessions/{id}/tasks/{task_id}", s.handleGetTask},
		{"POST /v1/sessions/{id}/tasks/{task_id}/cancel", s.handleCancelTask},
		{"GET /v1/info", s.handleInfo},
		{"GET /v1/openapi.yaml", s.handleOpenAPI},
		{"GET /healthz", s.handleHealthz},
		{"GET /metrics", s.handleMetrics},
	}
}

func (s *Server) buildMux() http.Handler {
	mux := http.NewServeMux()
	for _, e := range s.routeTable() {
		mux.HandleFunc(e.pattern, e.handler)
	}
	return s.instrument(mux)
}

// statusWriter records the status code written so middleware can log and
// measure it; Unwrap lets http.ResponseController reach the real writer
// (needed for Flush during SSE streaming).
type statusWriter struct {
	http.ResponseWriter
	status int
}

func (w *statusWriter) WriteHeader(status int) {
	w.status = status
	w.ResponseWriter.WriteHeader(status)
}

func (w *statusWriter) Unwrap() http.ResponseWriter { return w.ResponseWriter }

// instrument wraps every request with request-ID handling, structured
// logging, and HTTP metrics.
func (s *Server) instrument(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		id := requestID(r.Header.Get("X-Request-ID"))
		if id == "" {
			id, _ = newID()
		}
		w.Header().Set("X-Request-ID", id)

		r.Body = http.MaxBytesReader(w, r.Body, 1<<20)

		sw := &statusWriter{ResponseWriter: w, status: http.StatusOK}
		start := time.Now()
		next.ServeHTTP(sw, r)
		d := time.Since(start)

		route := r.Pattern
		if route == "" {
			route = "unmatched"
		}
		s.metrics.httpRequest(route, r.Method, sw.status, d)
		s.log.Info("http_request",
			"method", r.Method,
			"route", route,
			"status", sw.status,
			"duration_ms", d.Milliseconds(),
			"request_id", id,
		)
	})
}

// requestID reduces a client-supplied X-Request-ID to at most 64 printable
// ASCII bytes, per the design's trust-and-safety rule.
func requestID(raw string) string {
	if len(raw) > 64 {
		raw = raw[:64]
	}
	var b strings.Builder
	for _, r := range raw {
		if r >= 0x20 && r < 0x7f {
			b.WriteRune(r)
		}
	}
	return b.String()
}

// ---- error and JSON helpers ----

type apiError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

type errorBody struct {
	Error apiError `json:"error"`
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	b, err := json.Marshal(v)
	if err != nil {
		internalError(w, nil, err)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write(b)
}

func writeError(w http.ResponseWriter, status int, code, message string) {
	b, _ := json.Marshal(errorBody{Error: apiError{Code: code, Message: message}})
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write(b)
}

// internalError always sends the fixed 500 body; the real error, already
// redacted by the caller's factory, is only logged.
func internalError(w http.ResponseWriter, log *slog.Logger, err error) {
	if log != nil {
		log.Error("internal_error", "error", err)
	}
	writeError(w, http.StatusInternalServerError, "internal", "internal error")
}

// ---- session registry ----

// resumeOrCreate implements POST /v1/sessions. A hit in the registry
// returns the existing session without calling Open again; a miss on a
// resume request installs a placeholder under the lock so concurrent
// resumes of the same id call Open at most once (internal/session's
// PrepareListed opens the session file without an flock, so opening the
// same id twice would corrupt it).
func (s *Server) resumeOrCreate(ctx context.Context, id string) (os *openSession, created bool, err error) {
	if id == "" {
		ctrl, err := s.opts.Create(ctx)
		if err != nil {
			return nil, false, err
		}
		return s.register(ctrl), true, nil
	}

	s.mu.Lock()
	if existing, ok := s.sessions[id]; ok {
		s.mu.Unlock()
		return existing, false, nil
	}
	if placeholder, ok := s.opening[id]; ok {
		s.mu.Unlock()
		<-placeholder.ready
		return placeholder.sess, false, placeholder.err
	}
	placeholder := &openPlaceholder{ready: make(chan struct{})}
	s.opening[id] = placeholder
	s.mu.Unlock()

	ctrl, openErr := s.opts.Open(ctx, id)

	s.mu.Lock()
	delete(s.opening, id)
	if openErr != nil {
		placeholder.err = openErr
		s.mu.Unlock()
		close(placeholder.ready)
		return nil, false, openErr
	}
	sess := newOpenSession(ctrl)
	s.sessions[id] = sess
	placeholder.sess = sess
	s.mu.Unlock()
	close(placeholder.ready)

	s.metrics.sessionsOpen(1)
	s.startWakeLoop(sess)
	return sess, false, nil
}

func (s *Server) register(ctrl *app.Controller) *openSession {
	os := newOpenSession(ctrl)
	s.mu.Lock()
	s.sessions[ctrl.Info().SessionID] = os
	s.mu.Unlock()
	s.metrics.sessionsOpen(1)
	s.startWakeLoop(os)
	return os
}

// startWakeLoop starts a goroutine that automatically drains os's pending
// sub-agent task notifications. It is the sole caller of startTurn(os, "",
// triggerTask): both a os.ctrl.Tasks().Updates() signal and the end of any
// turn on os (via os.turnFinished, sent by startTurn) route through this one
// goroutine, so every "is a notification pending and no turn active" check
// happens one at a time — no two goroutines can race a stale check into
// starting two wake turns for the same notification. When a notification is
// pending and no turn is active, it starts a task-triggered wake turn (an
// active turn's own inbox drain handles the signal instead, so errTurnActive
// is expected and ignored). On every Updates() signal it also diffs
// Tasks().List() into the task-started/finished/running metrics. It returns
// immediately when the runner behind os.ctrl does not track tasks. It exits
// when Updates() closes (the controller closed) or s.ctx is done.
func (s *Server) startWakeLoop(os *openSession) {
	tasks := os.ctrl.Tasks()
	if tasks == nil {
		return
	}
	s.wakeWG.Add(1)
	go func() {
		defer s.wakeWG.Done()
		seen := make(map[string]agent.TaskStatus)
		for {
			select {
			case _, open := <-tasks.Updates():
				if !open {
					return
				}
				s.metrics.diffTasks(seen, tasks.List())
			case <-os.turnFinished:
			case <-s.ctx.Done():
				return
			}
			if _, err := s.startTurn(os, "", triggerTask); err != nil && !errors.Is(err, errTurnActive) {
				s.log.Error("wake_turn_error", "error", err)
			}
		}
	}()
}

func (s *Server) lookup(id string) (*openSession, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	os, ok := s.sessions[id]
	return os, ok
}

func (s *Server) remove(id string) (*openSession, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	os, ok := s.sessions[id]
	if ok {
		delete(s.sessions, id)
	}
	return os, ok
}

// ---- wire types ----

type sandboxWire struct {
	Mode          string `json:"mode"`
	Network       string `json:"network"`
	BashAvailable bool   `json:"bash_available"`
	Summary       string `json:"summary"`
}

type sessionTurnWire struct {
	ID      string `json:"id"`
	Trigger string `json:"trigger"`
	Status  string `json:"status"`
}

type sessionWire struct {
	ID                 string           `json:"id"`
	Workspace          string           `json:"workspace"`
	Provider           string           `json:"provider"`
	Profile            string           `json:"profile"`
	Model              string           `json:"model"`
	ContextWindow      int              `json:"context_window"`
	Usage              model.Usage      `json:"usage"`
	ContextInputTokens int              `json:"context_input_tokens"`
	Sandbox            sandboxWire      `json:"sandbox"`
	Turn               *sessionTurnWire `json:"turn"`
}

func (s *Server) sessionWire(os *openSession) sessionWire {
	info := os.ctrl.Info()
	os.mu.Lock()
	t := os.turn
	os.mu.Unlock()

	var turnWire *sessionTurnWire
	if t != nil {
		sum := t.summary()
		turnWire = &sessionTurnWire{ID: sum.ID, Trigger: sum.Trigger, Status: sum.Status}
	}

	return sessionWire{
		ID:                 info.SessionID,
		Workspace:          info.Workspace,
		Provider:           info.Provider,
		Profile:            info.Profile,
		Model:              info.Model,
		ContextWindow:      info.ContextWindow,
		Usage:              info.Usage,
		ContextInputTokens: info.ContextInputTokens,
		Sandbox: sandboxWire{
			Mode:          string(info.Sandbox.Mode),
			Network:       string(info.Sandbox.Network),
			BashAvailable: info.Sandbox.BashAvailable,
			Summary:       info.Sandbox.Summary(),
		},
		Turn: turnWire,
	}
}

type sessionListRow struct {
	ID        string `json:"id"`
	Path      string `json:"path,omitempty"`
	Workspace string `json:"workspace,omitempty"`
	Provider  string `json:"provider,omitempty"`
	Model     string `json:"model,omitempty"`
	Open      bool   `json:"open"`
}

type sessionListResponse struct {
	Sessions []sessionListRow `json:"sessions"`
}

type healthzWire struct {
	Status       string `json:"status"`
	SessionsOpen int    `json:"sessions_open"`
}

// ---- handlers ----

func (s *Server) handleCreateSession(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Resume string `json:"resume"`
	}
	if r.ContentLength != 0 {
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil && !errors.Is(err, io.EOF) {
			writeError(w, http.StatusBadRequest, "bad_request", "invalid JSON body")
			return
		}
	}

	os, created, err := s.resumeOrCreate(s.ctx, body.Resume)
	if err != nil {
		if errors.Is(err, ErrSessionNotFound) {
			writeError(w, http.StatusNotFound, "not_found", "session not found")
			return
		}
		internalError(w, s.log, err)
		return
	}

	status := http.StatusOK
	if created {
		status = http.StatusCreated
	}
	writeJSON(w, status, s.sessionWire(os))
}

func (s *Server) handleListSessions(w http.ResponseWriter, r *http.Request) {
	var disk session.ListResult
	if s.opts.List != nil {
		var err error
		disk, err = s.opts.List(r.Context())
		if err != nil {
			internalError(w, s.log, err)
			return
		}
	}

	s.mu.Lock()
	openIDs := make(map[string]*openSession, len(s.sessions))
	for id, os := range s.sessions {
		openIDs[id] = os
	}
	s.mu.Unlock()

	rows := make([]sessionListRow, 0, len(disk.Sessions)+len(openIDs))
	seen := make(map[string]bool, len(disk.Sessions))
	for _, info := range disk.Sessions {
		seen[info.ID] = true
		_, open := openIDs[info.ID]
		rows = append(rows, sessionListRow{
			ID:        info.ID,
			Path:      info.Path,
			Workspace: info.CWD,
			Provider:  info.Provider,
			Model:     info.Model,
			Open:      open,
		})
	}
	for id, os := range openIDs {
		if seen[id] {
			continue
		}
		info := os.ctrl.Info()
		rows = append(rows, sessionListRow{
			ID:        id,
			Workspace: info.Workspace,
			Provider:  info.Provider,
			Model:     info.Model,
			Open:      true,
		})
	}

	writeJSON(w, http.StatusOK, sessionListResponse{Sessions: rows})
}

func (s *Server) handleGetSession(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	writeJSON(w, http.StatusOK, s.sessionWire(os))
}

func (s *Server) handleDeleteSession(w http.ResponseWriter, r *http.Request) {
	os, ok := s.remove(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}

	os.mu.Lock()
	if os.turn != nil {
		os.turn.cancel()
	}
	os.mu.Unlock()

	if err := os.ctrl.Close(); err != nil {
		s.log.Error("session_close_error", "session_id", r.PathValue("id"), "error", err)
	}
	s.metrics.sessionsOpen(-1)
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) handleHistory(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	history := os.ctrl.History()
	if history == nil {
		history = []model.Message{} // JSON "[]", not "null"
	}
	writeJSON(w, http.StatusOK, history)
}

// errTurnActive is returned by startTurn when the session already has a
// turn running.
var errTurnActive = errors.New("turn already active")

// startTurn starts a turn on os with the given trigger ("user" or "task")
// and returns it. Task turns prepare their controller wake operation before
// publishing os.turn, so a no-op or busy admission cannot create a phantom
// visible turn. The turn runs in its own goroutine; once it finishes, it
// signals os.turnFinished so startWakeLoop can retry a late notification.
func (s *Server) startTurn(os *openSession, text, trigger string) (*turn, error) {
	os.mu.Lock()
	if os.turn != nil && !os.turn.isDone() {
		os.mu.Unlock()
		return nil, errTurnActive
	}
	turnID, err := newID()
	if err != nil {
		os.mu.Unlock()
		return nil, err
	}
	turnCtx, cancel := context.WithCancel(s.ctx)
	var wake *app.WakeOperation
	if trigger == triggerTask {
		wake, err = os.ctrl.PrepareWake(turnCtx)
		if err != nil {
			cancel()
			os.mu.Unlock()
			if errors.Is(err, app.ErrPromptActive) {
				return nil, errTurnActive
			}
			return nil, err
		}
		if wake == nil {
			cancel()
			os.mu.Unlock()
			return nil, nil
		}
	}
	t := newTurn(turnID, cancel)
	t.trigger = trigger
	os.turn = t
	os.mu.Unlock()

	s.metrics.turnStarted()
	emit := t.emit(s.metrics)
	go func() {
		defer cancel()
		var err error
		if wake != nil {
			err = wake.Run(turnCtx, emit)
		} else {
			err = os.ctrl.Prompt(turnCtx, text, emit)
		}
		t.finish(err)
		s.metrics.turnFinished(t.summary().Status, t.duration())
		if err != nil && !errors.Is(err, context.Canceled) {
			s.log.Error("turn_error", "turn_id", turnID, "trigger", trigger, "error", err)
		}
		s.log.Info("turn_finished", "turn_id", turnID, "trigger", trigger, "status", t.summary().Status, "duration_ms", t.duration().Milliseconds())

		select {
		case os.turnFinished <- struct{}{}:
		default:
		}
	}()

	s.log.Info("turn_started", "turn_id", turnID, "trigger", trigger)
	return t, nil
}

func (s *Server) handleStartTurn(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}

	var body struct {
		Text   string `json:"text"`
		Stream *bool  `json:"stream"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "bad_request", "invalid JSON body")
		return
	}
	if strings.TrimSpace(body.Text) == "" {
		writeError(w, http.StatusBadRequest, "bad_request", "text must not be empty")
		return
	}
	stream := true
	if body.Stream != nil {
		stream = *body.Stream
	}

	t, err := s.startTurn(os, body.Text, triggerUser)
	if err != nil {
		if errors.Is(err, errTurnActive) {
			writeError(w, http.StatusConflict, "turn_active", "a turn is already active for this session")
			return
		}
		internalError(w, s.log, err)
		return
	}

	if stream {
		s.streamSSE(w, r, t, 0)
		return
	}

	waitDone(t)
	writeJSON(w, http.StatusOK, t.summary())
}

// waitDone blocks until t is done, without polling, by riding the same
// changed-channel broadcast the SSE reader uses.
func waitDone(t *turn) {
	after := 0
	for {
		events, done, changed := t.snapshot(after)
		after += len(events)
		if done {
			return
		}
		<-changed
	}
}

func (s *Server) handleGetTurn(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	os.mu.Lock()
	t := os.turn
	os.mu.Unlock()
	if t == nil || t.id != r.PathValue("turn_id") {
		writeError(w, http.StatusNotFound, "not_found", "turn not found")
		return
	}
	writeJSON(w, http.StatusOK, t.summary())
}

func (s *Server) handleTurnEvents(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	os.mu.Lock()
	t := os.turn
	os.mu.Unlock()
	if t == nil || t.id != r.PathValue("turn_id") {
		writeError(w, http.StatusNotFound, "not_found", "turn not found")
		return
	}

	after := 0
	if v := r.URL.Query().Get("after"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil || n < 0 {
			writeError(w, http.StatusBadRequest, "bad_request", "after must be a non-negative integer")
			return
		}
		after = n + 1
	} else if v := r.Header.Get("Last-Event-ID"); v != "" {
		n, err := strconv.Atoi(v)
		if err == nil && n >= 0 {
			after = n + 1
		}
	}

	s.streamSSE(w, r, t, after)
}

func (s *Server) handleCancelTurn(w http.ResponseWriter, r *http.Request) {
	os, ok := s.lookup(r.PathValue("id"))
	if !ok {
		writeError(w, http.StatusNotFound, "not_found", "session not found")
		return
	}
	os.mu.Lock()
	t := os.turn
	os.mu.Unlock()
	if t == nil || t.id != r.PathValue("turn_id") {
		writeError(w, http.StatusNotFound, "not_found", "turn not found")
		return
	}
	t.cancel()
	w.WriteHeader(http.StatusAccepted)
}

func (s *Server) handleMetrics(w http.ResponseWriter, r *http.Request) {
	s.metrics.replaceSessionContexts(s.sessionContextMetrics())
	s.metrics.ServeHTTP(w, r)
}

func (s *Server) sessionContextMetrics() []sessionContextMetrics {
	s.mu.Lock()
	sessions := make([]*openSession, 0, len(s.sessions))
	for _, os := range s.sessions {
		sessions = append(sessions, os)
	}
	s.mu.Unlock()

	samples := make([]sessionContextMetrics, 0, len(sessions))
	for _, os := range sessions {
		info := os.ctrl.Info()
		samples = append(samples, sessionContextMetrics{
			SessionID:                 info.SessionID,
			Provider:                  info.Provider,
			Model:                     info.Model,
			ContextWindow:             info.ContextWindow,
			ContextInputTokens:        info.ContextInputTokens,
			ContextInputTokensPresent: info.ContextInputTokensPresent,
			ContextInputTokensPending: info.ContextInputTokensPending,
		})
	}
	return samples
}

func (s *Server) handleInfo(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, s.opts.Info)
}

func (s *Server) handleOpenAPI(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/yaml")
	_, _ = w.Write(openapiYAML)
}

func (s *Server) handleHealthz(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	n := len(s.sessions)
	s.mu.Unlock()
	writeJSON(w, http.StatusOK, healthzWire{Status: "ok", SessionsOpen: n})
}

// ---- SSE streaming ----

// streamSSE writes turn events from seq after onward as SSE frames, then
// blocks for more until the turn finishes or the client disconnects.
// Disconnecting never cancels the turn: it just stops this handler.
func (s *Server) streamSSE(w http.ResponseWriter, r *http.Request, t *turn, after int) {
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.WriteHeader(http.StatusOK)

	rc := http.NewResponseController(w)
	s.metrics.streamClients(1)
	defer s.metrics.streamClients(-1)

	for {
		events, done, changed := t.snapshot(after)
		for i, e := range events {
			seq := after + i
			if err := writeSSEFrame(w, seq, e); err != nil {
				return
			}
		}
		after += len(events)
		if len(events) > 0 {
			if err := rc.Flush(); err != nil {
				return
			}
		}
		if done {
			return
		}
		select {
		case <-changed:
		case <-r.Context().Done():
			return
		}
	}
}

func writeSSEFrame(w http.ResponseWriter, seq int, e wireEvent) error {
	data, err := json.Marshal(e)
	if err != nil {
		return err
	}
	_, err = fmt.Fprintf(w, "id: %d\nevent: %s\ndata: %s\n\n", seq, e.Type, data)
	return err
}
