// Package fake is a scriptable ACP agent for tests. A test binary calls
// Main from TestMain; when the binary was started as the agent (see Env) Main
// serves ACP on stdin/stdout and never returns, otherwise it returns false.
//
// Behavior is chosen by the prompt text:
//
//	remember     a tool call titled "remember", then "Queued."
//	chunks       "Hello", a thought, " world", a tool call, "After tool"
//	block        waits for session/cancel, then ends with stop reason cancelled
//	sleep        waits 300 ms, then echoes
//	hold:<n>     waits until the session has received n task-control or
//	             steer requests (or session/cancel), then echoes; a test
//	             that acts on an active task uses it instead of racing sleep
//	wait         waits until the test calls Release (or session/cancel),
//	             then echoes; for a task the agent hears nothing about
//	perm:<title> requests permission for a tool call titled <title>; ends
//	             with stop reason cancelled when the outcome is cancelled,
//	             otherwise replies "outcome=<option id>"
//	permcancel:<title>
//	             like perm:, but the agent cancels its permission request
//	             after 200 ms, which sends $/cancel_request
//	exit         writes "fatal: boom" to stderr and exits with status 3
//	other        replies "echo: <text>"
//
// The agent advertises _meta.otto.memoryReview unless FAKE_NO_MEMORY=1 and
// serves _otto/memory/pending and _otto/memory/review over the candidates in
// <dir>/memory (FAKE_PAGE=<n> pages pending n at a time, FAKE_MEMORY_UNAVAILABLE=1
// answers it with -32010; a candidate id starting with "dddd" conflicts on
// review), one "<id>\t<kind>\t<text>" per line. A review removes the
// line and is logged as review:<id>:<decision>.
// It advertises _meta.otto.taskControl unless FAKE_NO_TASK_CONTROL=1 and
// serves _otto/tasks/control and _otto/tasks/steer while a prompt is active.
// FAKE_NO_TASK_CONTROL_METHODS=1 keeps the capability but returns method-not-found.
//
// "ext:<method> <json>" calls that agent-to-client extension method.
//
// Every call is appended to <dir>/calls as one line: init:<pid>, new:<id>,
// load:<id>, start:<text>, end:<text>, cancel:<id>, perm:<outcome>,
// task_control:<text>, and steer:<text>.
// Sessions persist in <dir>/sessions so a restarted agent can load them;
// loading an unknown id fails. A line is "<id>" or
// "<id>\t<updatedAt>\t<title>"; session/list returns the lines newest
// (last) first. FAKE_REPLAY=<n> makes session/load replay n
// updates, FAKE_NO_EXIT=1 makes the process ignore end of stdin.
package fake

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/coder/acp-go-sdk"
)

const envDir = "OTTO_CONNECT_FAKE_AGENT"

// Env returns the environment that makes the test binary run as the agent
// with state in dir. extra entries (KEY=value) are appended.
//
// Under -race the fake agent is a race-instrumented binary, and the race
// runtime sleeps atexit_sleep_ms (default 1 s) before every exit; the
// harness waits for that exit on every shutdown, so the sleep is turned off.
// Any GORACE options the caller set are kept.
func Env(dir string, extra ...string) []string {
	gorace := strings.TrimSpace(os.Getenv("GORACE") + " atexit_sleep_ms=0")
	return append(append(os.Environ(), envDir+"="+dir, "GORACE="+gorace), extra...)
}

// Calls returns the lines of <dir>/calls.
func Calls(dir string) []string {
	raw, _ := os.ReadFile(dir + "/calls")
	if len(raw) == 0 {
		return nil
	}
	return strings.Split(strings.TrimSpace(string(raw)), "\n")
}

// Release ends the "wait" prompts of the agent with state in dir.
func Release(dir string) error {
	return os.WriteFile(dir+"/release", nil, 0o600)
}

// Main runs the agent when the environment selects it.
func Main() bool {
	dir := os.Getenv(envDir)
	if dir == "" {
		return false
	}
	a := &agent{dir: dir, cancels: map[string]chan struct{}{}, approvals: map[string]bool{}, controls: map[string]chan struct{}{}}
	conn := acp.NewAgentSideConnection(a, os.Stdout, os.Stdin)
	a.conn.Store(conn)
	<-conn.Done()
	// Done closes when stdin ends, before the SDK has run the notifications
	// it already read. Wait for blocked prompts so a session/cancel sent just
	// before stdin closed is still handled and logged.
	for deadline := time.Now().Add(time.Second); a.blockedCount() > 0 && time.Now().Before(deadline); {
		time.Sleep(time.Millisecond)
	}
	if os.Getenv("FAKE_NO_EXIT") != "" {
		// Keep a timer pending so the runtime does not report a deadlock.
		for {
			time.Sleep(time.Hour)
		}
	}
	os.Exit(0)
	return true
}

type agent struct {
	acp.Agent // unused methods panic

	dir  string
	conn atomic.Pointer[acp.AgentSideConnection] // set after the SDK starts reading

	mu        sync.Mutex
	policyMu  sync.Mutex
	cancels   map[string]chan struct{} // current prompt of each session
	blocked   int                      // "block" prompts waiting for session/cancel
	approvals map[string]bool
	controls  map[string]chan struct{} // signalled per control request to a "hold:<n>" prompt
}

// controlled signals a "hold:<n>" prompt of session that a task-control or
// steer request arrived.
func (a *agent) controlled(session string) {
	a.mu.Lock()
	ch := a.controls[session]
	a.mu.Unlock()
	if ch != nil {
		select {
		case ch <- struct{}{}:
		default:
		}
	}
}

func (a *agent) log(format string, args ...any) {
	f, err := os.OpenFile(a.dir+"/calls", os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	fmt.Fprintf(f, format+"\n", args...)
}

func (a *agent) blockedCount() int {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.blocked
}

func (a *agent) known(id string) bool {
	f, err := os.Open(a.dir + "/sessions")
	if err != nil {
		return false
	}
	defer f.Close()
	s := bufio.NewScanner(f)
	for s.Scan() {
		if strings.SplitN(s.Text(), "\t", 2)[0] == id {
			return true
		}
	}
	return false
}

func (a *agent) Initialize(_ context.Context, req acp.InitializeRequest) (acp.InitializeResponse, error) {
	a.log("init:%d", os.Getpid())
	if otto, ok := req.ClientCapabilities.Meta["otto"].(map[string]any); ok {
		raw, _ := json.Marshal(otto["scheduleDelivery"])
		a.log("schedule_delivery:%s", raw)
	}
	caps := acp.AgentCapabilities{LoadSession: true, SessionCapabilities: acp.SessionCapabilities{List: &acp.SessionListCapabilities{}}}
	meta := map[string]any{}
	if os.Getenv("FAKE_APPROVAL_POLICY") != "" {
		meta["approvalPolicy"] = map[string]any{"version": 1}
	}
	if os.Getenv("FAKE_NO_MEMORY") == "" {
		meta["memoryReview"] = true
	}
	if os.Getenv("FAKE_APPROVAL_DIALOGUE") != "" {
		meta["approvalDialogue"] = true
	}
	if os.Getenv("FAKE_NO_CHANNEL_CONTEXT") == "" {
		meta["channelContext"] = true
	}
	if os.Getenv("FAKE_NO_TASK_CONTROL") == "" {
		meta["taskControl"] = true
	}
	if len(meta) > 0 {
		caps.Meta = map[string]any{"otto": meta}
	}
	return acp.InitializeResponse{
		ProtocolVersion:   acp.ProtocolVersionNumber,
		AgentCapabilities: caps,
		AuthMethods:       []acp.AuthMethod{},
	}, nil
}

func (a *agent) Authenticate(context.Context, acp.AuthenticateRequest) (acp.AuthenticateResponse, error) {
	return acp.AuthenticateResponse{}, nil
}

func (a *agent) NewSession(_ context.Context, p acp.NewSessionRequest) (acp.NewSessionResponse, error) {
	if p.Cwd == "" || p.McpServers == nil {
		return acp.NewSessionResponse{}, errors.New("cwd and mcpServers are required")
	}
	b := make([]byte, 16)
	_, _ = rand.Read(b)
	id := hex.EncodeToString(b)
	f, err := os.OpenFile(a.dir+"/sessions", os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return acp.NewSessionResponse{}, err
	}
	fmt.Fprintln(f, id)
	f.Close()
	a.log("new:%s", id)
	return acp.NewSessionResponse{SessionId: acp.SessionId(id)}, nil
}

func (a *agent) LoadSession(ctx context.Context, p acp.LoadSessionRequest) (acp.LoadSessionResponse, error) {
	id := string(p.SessionId)
	a.log("load:%s", id)
	if !a.known(id) {
		return acp.LoadSessionResponse{}, errors.New("unknown session")
	}
	n, _ := strconv.Atoi(os.Getenv("FAKE_REPLAY"))
	for i := 0; i < n; i++ {
		text := acp.UpdateAgentMessageText("OLD")
		if i%2 == 0 {
			text = acp.UpdateUserMessageText("OLD")
		}
		if err := a.conn.Load().SessionUpdate(ctx, acp.SessionNotification{SessionId: p.SessionId, Update: text}); err != nil {
			return acp.LoadSessionResponse{}, err
		}
	}
	return acp.LoadSessionResponse{}, nil
}

func (a *agent) ListSessions(_ context.Context, p acp.ListSessionsRequest) (acp.ListSessionsResponse, error) {
	if p.Cwd == nil || *p.Cwd == "" {
		return acp.ListSessionsResponse{}, errors.New("cwd is required")
	}
	raw, _ := os.ReadFile(a.dir + "/sessions")
	lines := strings.Split(strings.TrimSpace(string(raw)), "\n")
	out := []acp.SessionInfo{}
	for i := len(lines) - 1; i >= 0 && lines[i] != ""; i-- {
		f := strings.SplitN(lines[i], "\t", 3)
		info := acp.SessionInfo{SessionId: acp.SessionId(f[0]), Cwd: *p.Cwd}
		if len(f) == 3 {
			info.UpdatedAt, info.Title = &f[1], &f[2]
		}
		out = append(out, info)
	}
	return acp.ListSessionsResponse{Sessions: out}, nil
}

func (a *agent) Cancel(_ context.Context, p acp.CancelNotification) error {
	a.log("cancel:%s", p.SessionId)
	a.mu.Lock()
	defer a.mu.Unlock()
	if ch := a.cancels[string(p.SessionId)]; ch != nil {
		close(ch)
		delete(a.cancels, string(p.SessionId))
	}
	return nil
}

func (a *agent) chunk(ctx context.Context, id acp.SessionId, text string) {
	_ = a.conn.Load().SessionUpdate(ctx, acp.SessionNotification{SessionId: id, Update: acp.UpdateAgentMessageText(text)})
}

func (a *agent) Prompt(ctx context.Context, p acp.PromptRequest) (acp.PromptResponse, error) {
	text := p.Prompt[0].Text.Text
	id := string(p.SessionId)
	// The prompt is active, and a "hold:<n>" prompt listens for control
	// requests, before start is logged, since a test acts on an active task
	// as soon as it sees start.
	cancelled := make(chan struct{})
	a.mu.Lock()
	a.cancels[id] = cancelled
	a.mu.Unlock()
	defer func() {
		a.mu.Lock()
		if a.cancels[id] == cancelled {
			delete(a.cancels, id)
		}
		a.mu.Unlock()
	}()
	var signal chan struct{}
	if strings.HasPrefix(text, "hold:") {
		signal = make(chan struct{}, 16)
		a.mu.Lock()
		a.controls[id] = signal
		a.mu.Unlock()
		defer func() {
			a.mu.Lock()
			delete(a.controls, id)
			a.mu.Unlock()
		}()
	}
	a.log("start:%s", text)
	if channel := channelFromMeta(p.Meta); channel != "" {
		a.log("channel:%s", channel)
	}
	if otto, ok := p.Meta["otto"].(map[string]any); ok {
		if chat, _ := otto["chat"].(string); chat != "" {
			a.log("chat:%s", chat)
		}
		if id, _ := otto["requestId"].(string); id != "" {
			a.log("request_id:%s", id)
		}
		if id, _ := otto["inputId"].(string); id != "" {
			a.log("input_id:%s", id)
		}
	}
	defer func() { a.log("end:%s", text) }()

	end := acp.PromptResponse{StopReason: acp.StopReasonEndTurn}
	switch {
	case text == "chunks":
		a.chunk(ctx, p.SessionId, "Hello")
		_ = a.conn.Load().SessionUpdate(ctx, acp.SessionNotification{SessionId: p.SessionId, Update: acp.UpdateAgentThoughtText("thinking")})
		a.chunk(ctx, p.SessionId, " world")
		_ = a.conn.Load().SessionUpdate(ctx, acp.SessionNotification{SessionId: p.SessionId, Update: acp.StartToolCall("t1", "ls")})
		a.chunk(ctx, p.SessionId, "After tool")
	case text == "remember":
		_ = a.conn.Load().SessionUpdate(ctx, acp.SessionNotification{SessionId: p.SessionId, Update: acp.StartToolCall("t2", "remember")})
		a.chunk(ctx, p.SessionId, "Queued.")
	case text == "block":
		a.mu.Lock()
		a.blocked++
		a.mu.Unlock()
		<-cancelled
		a.mu.Lock()
		a.blocked--
		a.mu.Unlock()
		return acp.PromptResponse{StopReason: acp.StopReasonCancelled}, nil
	case text == "sleep":
		select {
		case <-time.After(300 * time.Millisecond):
		case <-cancelled:
			return acp.PromptResponse{StopReason: acp.StopReasonCancelled}, nil
		}
		a.chunk(ctx, p.SessionId, "echo: "+text)
	case text == "wait":
		for {
			if _, err := os.Stat(a.dir + "/release"); err == nil {
				break
			}
			select {
			case <-time.After(5 * time.Millisecond):
			case <-cancelled:
				return acp.PromptResponse{StopReason: acp.StopReasonCancelled}, nil
			}
		}
		a.chunk(ctx, p.SessionId, "echo: "+text)
	case strings.HasPrefix(text, "hold:"):
		n, err := strconv.Atoi(strings.TrimPrefix(text, "hold:"))
		if err != nil {
			return end, err
		}
		for ; n > 0; n-- {
			select {
			case <-signal:
			case <-cancelled:
				return acp.PromptResponse{StopReason: acp.StopReasonCancelled}, nil
			}
		}
		a.chunk(ctx, p.SessionId, "echo: "+text)
	case strings.HasPrefix(text, "perm:"), strings.HasPrefix(text, "permcancel:"), strings.HasPrefix(text, "permread:"):
		title := text[strings.Index(text, ":")+1:]
		reqCtx := ctx
		if strings.HasPrefix(text, "permcancel:") {
			var stop context.CancelFunc
			reqCtx, stop = context.WithCancel(ctx)
			defer stop()
			time.AfterFunc(200*time.Millisecond, stop)
		}
		allowOption := acp.PermissionOption{Kind: acp.PermissionOptionKindAllowOnce, Name: "Allow once", OptionId: "allow_once"}
		if strings.HasPrefix(text, "permread:") {
			allowOption = acp.PermissionOption{Kind: acp.PermissionOptionKindAllowAlways, Name: "Save read access", OptionId: "read_grant"}
		}
		a.mu.Lock()
		a.approvals[id] = true
		a.mu.Unlock()
		resp, err := a.conn.Load().RequestPermission(reqCtx, acp.RequestPermissionRequest{
			SessionId: p.SessionId,
			ToolCall:  acp.ToolCallUpdate{ToolCallId: "t1", Title: &title},
			Options: []acp.PermissionOption{
				allowOption,
				{Kind: acp.PermissionOptionKindRejectOnce, Name: "Deny", OptionId: "reject_once"},
			},
		})
		a.mu.Lock()
		delete(a.approvals, id)
		a.mu.Unlock()
		// The SDK ends the agent's request context when session/cancel
		// arrives; that is an outcome of cancelled.
		if err != nil && reqCtx.Err() == nil {
			return end, err
		}
		outcome := "cancelled"
		if err == nil && resp.Outcome.Selected != nil {
			outcome = string(resp.Outcome.Selected.OptionId)
		}
		a.log("perm:%s", outcome)
		if outcome == "cancelled" {
			return acp.PromptResponse{StopReason: acp.StopReasonCancelled}, nil
		}
		a.chunk(ctx, p.SessionId, "outcome="+outcome)
	case strings.HasPrefix(text, "ext:"):
		// ext:<method> <json params> calls an agent-to-client method and
		// logs ext_result:<error or result>.
		method, params, _ := strings.Cut(text[len("ext:"):], " ")
		raw, err := a.conn.Load().CallExtension(ctx, method, json.RawMessage(params))
		if err != nil {
			a.log("ext_result:error:%s", err)
		} else {
			a.log("ext_result:%s", raw)
		}
	case text == "exit":
		fmt.Fprintln(os.Stderr, "fatal: boom")
		os.Exit(3)
	default:
		a.chunk(ctx, p.SessionId, "echo: "+text)
	}
	return end, nil
}

// HandleExtensionMethod serves the _otto/memory/* methods.
func (a *agent) HandleExtensionMethod(_ context.Context, method string, params json.RawMessage) (any, error) {
	path := a.dir + "/memory"
	raw, _ := os.ReadFile(path)
	var lines []string
	if s := strings.TrimSpace(string(raw)); s != "" {
		lines = strings.Split(s, "\n")
	}
	switch method {
	case "_otto/approval_policy/get", "_otto/approval_policy/set":
		if os.Getenv("FAKE_APPROVAL_POLICY") == "" {
			return nil, &acp.RequestError{Code: -32601, Message: "unsupported approval policy"}
		}
		return a.policyMethod(method, params)
	case "_otto/approval_mode/get", "_otto/approval_mode/set":
		if os.Getenv("FAKE_APPROVAL_POLICY") != "" {
			return a.policyMethod(method, params)
		}
		var input struct {
			SessionID string `json:"sessionId"`
			Automatic *bool  `json:"automatic"`
		}
		if err := json.Unmarshal(params, &input); err != nil || input.SessionID == "" {
			return nil, errors.New("invalid session mode")
		}
		path := a.dir + "/mode-" + input.SessionID
		if method == "_otto/approval_mode/set" {
			if input.Automatic == nil {
				return nil, errors.New("automatic required")
			}
			if err := os.WriteFile(path, []byte(strconv.FormatBool(*input.Automatic)), 0600); err != nil {
				return nil, err
			}
			a.log("auto:%s:%t", input.SessionID, *input.Automatic)
		}
		value, err := os.ReadFile(path)
		return map[string]bool{"automatic": err != nil || string(value) == "true"}, nil
	case "_otto/memory/pending":
		if os.Getenv("FAKE_MEMORY_UNAVAILABLE") != "" {
			return nil, &acp.RequestError{Code: -32010, Message: "memory is not available in this session"}
		}
		var req struct{ Cursor string }
		_ = json.Unmarshal(params, &req)
		start, _ := strconv.Atoi(req.Cursor)
		next := ""
		if n, _ := strconv.Atoi(os.Getenv("FAKE_PAGE")); n > 0 {
			end := min(start+n, len(lines))
			if end < len(lines) {
				next = strconv.Itoa(end)
			}
			lines = lines[min(start, len(lines)):end]
		}
		out := []map[string]string{}
		for _, l := range lines {
			f := strings.SplitN(l, "\t", 4)
			c := map[string]string{"id": f[0], "action": "create", "kind": f[1], "key": "k", "text": f[2], "reason": "because", "origin": "model"}
			if len(f) > 3 {
				c["advice"] = f[3] // optional fourth seed column
			}
			out = append(out, c)
		}
		return map[string]any{"candidates": out, "nextCursor": next}, nil
	case "_otto/memory/review":
		var req struct{ CandidateID, Decision string }
		if err := json.Unmarshal(params, &req); err != nil {
			return nil, err
		}
		var keep []string
		found := false
		for _, l := range lines {
			if strings.SplitN(l, "\t", 2)[0] == req.CandidateID {
				found = true
				continue
			}
			keep = append(keep, l)
		}
		if !found {
			return nil, &acp.RequestError{Code: -32002, Message: "candidate " + req.CandidateID + " not found"}
		}
		if strings.HasPrefix(req.CandidateID, "dddd") {
			return nil, &acp.RequestError{Code: -32011, Message: "memory revision conflict"}
		}
		if err := os.WriteFile(path, []byte(strings.Join(keep, "\n")+"\n"), 0o600); err != nil {
			return nil, err
		}
		a.log("review:%s:%s", req.CandidateID, req.Decision)
		out := map[string]any{"decision": req.Decision, "candidateId": req.CandidateID}
		if req.Decision == "accept" {
			out["record"] = map[string]any{"id": "rec-" + req.CandidateID, "revision": 1}
		}
		return out, nil
	case "_otto/approvals/message":
		var req struct {
			SessionID string `json:"sessionId"`
			Text      string `json:"text"`
			Channel   string `json:"channel"`
		}
		if err := json.Unmarshal(params, &req); err != nil {
			return nil, err
		}
		a.mu.Lock()
		pending := a.approvals[req.SessionID]
		a.mu.Unlock()
		if !pending {
			return nil, nil
		}
		a.log("approval:%s", req.Text)
		if req.Channel != "" {
			a.log("approval_channel:%s", req.Channel)
		}
		if req.Text == "Why do you need this?" {
			return map[string]any{"text": "The agent needs approval to run this tool call.", "queued": false}, nil
		}
		return map[string]any{"text": "I will handle that after approval.", "queued": true}, nil
	case "_otto/tasks/control":
		if os.Getenv("FAKE_NO_TASK_CONTROL_METHODS") != "" {
			return nil, acp.NewMethodNotFound(method)
		}
		var req struct {
			SessionID string `json:"sessionId"`
			Text      string `json:"text"`
			Channel   string `json:"channel"`
		}
		if err := json.Unmarshal(params, &req); err != nil {
			return nil, err
		}
		a.mu.Lock()
		active := a.cancels[req.SessionID] != nil && !a.approvals[req.SessionID]
		a.mu.Unlock()
		a.log("task_control:%s", req.Text)
		if req.Channel != "" {
			a.log("task_control_channel:%s", req.Channel)
		}
		defer a.controlled(req.SessionID)
		if strings.HasPrefix(req.Text, "slow-follow-up: ") || req.Text == "hold-control" {
			time.Sleep(200 * time.Millisecond)
		}
		if !active || req.Text == "no-decision" {
			return nil, nil
		}
		if req.Text == "control-error" {
			return nil, &acp.RequestError{Code: -32020, Message: "task control failed"}
		}
		if strings.HasPrefix(req.Text, "follow-up: ") {
			return map[string]any{"text": "I will handle that after the current task.", "queued": true}, nil
		}
		return map[string]any{"text": "Guidance accepted.", "queued": false}, nil
	case "_otto/tasks/steer":
		if os.Getenv("FAKE_NO_TASK_CONTROL_METHODS") != "" {
			return nil, acp.NewMethodNotFound(method)
		}
		var req struct {
			SessionID string `json:"sessionId"`
			Text      string `json:"text"`
		}
		if err := json.Unmarshal(params, &req); err != nil {
			return nil, err
		}
		a.mu.Lock()
		active := a.cancels[req.SessionID] != nil && !a.approvals[req.SessionID]
		a.mu.Unlock()
		if !active {
			return nil, &acp.RequestError{Code: -32021, Message: "no active task"}
		}
		a.log("steer:%s", req.Text)
		a.controlled(req.SessionID)
		return map[string]any{"accepted": true}, nil
	}
	return nil, acp.NewMethodNotFound(method)
}

func channelFromMeta(meta map[string]any) string {
	otto, _ := meta["otto"].(map[string]any)
	channel, _ := otto["channel"].(string)
	return channel
}
