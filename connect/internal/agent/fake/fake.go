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
//
// Every call is appended to <dir>/calls as one line: init:<pid>, new:<id>,
// load:<id>, start:<text>, end:<text>, cancel:<id>, perm:<outcome>.
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
func Env(dir string, extra ...string) []string {
	return append(append(os.Environ(), envDir+"="+dir), extra...)
}

// Calls returns the lines of <dir>/calls.
func Calls(dir string) []string {
	raw, _ := os.ReadFile(dir + "/calls")
	if len(raw) == 0 {
		return nil
	}
	return strings.Split(strings.TrimSpace(string(raw)), "\n")
}

// Main runs the agent when the environment selects it.
func Main() bool {
	dir := os.Getenv(envDir)
	if dir == "" {
		return false
	}
	a := &agent{dir: dir, cancels: map[string]chan struct{}{}}
	conn := acp.NewAgentSideConnection(a, os.Stdout, os.Stdin)
	a.conn.Store(conn)
	<-conn.Done()
	if os.Getenv("FAKE_NO_EXIT") != "" {
		select {}
	}
	os.Exit(0)
	return true
}

type agent struct {
	acp.Agent // unused methods panic

	dir  string
	conn atomic.Pointer[acp.AgentSideConnection] // set after the SDK starts reading

	mu      sync.Mutex
	cancels map[string]chan struct{} // current prompt of each session
}

func (a *agent) log(format string, args ...any) {
	f, err := os.OpenFile(a.dir+"/calls", os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	fmt.Fprintf(f, format+"\n", args...)
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

func (a *agent) Initialize(context.Context, acp.InitializeRequest) (acp.InitializeResponse, error) {
	a.log("init:%d", os.Getpid())
	caps := acp.AgentCapabilities{LoadSession: true, SessionCapabilities: acp.SessionCapabilities{List: &acp.SessionListCapabilities{}}}
	if os.Getenv("FAKE_NO_MEMORY") == "" {
		caps.Meta = map[string]any{"otto": map[string]any{"memoryReview": true}}
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
	a.log("start:%s", text)
	defer func() { a.log("end:%s", text) }()
	cancelled := make(chan struct{})
	a.mu.Lock()
	a.cancels[id] = cancelled
	a.mu.Unlock()

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
		<-cancelled
		return acp.PromptResponse{StopReason: acp.StopReasonCancelled}, nil
	case text == "sleep":
		select {
		case <-time.After(300 * time.Millisecond):
		case <-cancelled:
			return acp.PromptResponse{StopReason: acp.StopReasonCancelled}, nil
		}
		a.chunk(ctx, p.SessionId, "echo: "+text)
	case strings.HasPrefix(text, "perm:"), strings.HasPrefix(text, "permcancel:"):
		title := text[strings.Index(text, ":")+1:]
		reqCtx := ctx
		if strings.HasPrefix(text, "permcancel:") {
			var stop context.CancelFunc
			reqCtx, stop = context.WithCancel(ctx)
			defer stop()
			time.AfterFunc(200*time.Millisecond, stop)
		}
		resp, err := a.conn.Load().RequestPermission(reqCtx, acp.RequestPermissionRequest{
			SessionId: p.SessionId,
			ToolCall:  acp.ToolCallUpdate{ToolCallId: "t1", Title: &title},
			Options: []acp.PermissionOption{
				{Kind: acp.PermissionOptionKindAllowOnce, Name: "Allow once", OptionId: "allow_once"},
				{Kind: acp.PermissionOptionKindRejectOnce, Name: "Deny", OptionId: "reject_once"},
			},
		})
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
			f := strings.SplitN(l, "\t", 3)
			out = append(out, map[string]string{"id": f[0], "action": "create", "kind": f[1], "key": "k", "text": f[2], "reason": "because", "origin": "model"})
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
	}
	return nil, acp.NewMethodNotFound(method)
}
