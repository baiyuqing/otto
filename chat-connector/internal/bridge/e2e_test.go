package bridge

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/baiyuqing/otto/chat-connector/internal/state"
)

// lastUserText returns the text of the last user message of a chat
// completions request body.
func lastUserText(body []byte) string {
	texts := userTexts(body)
	if len(texts) == 0 {
		return ""
	}
	return texts[len(texts)-1]
}

func userTexts(body []byte) []string {
	var req struct {
		Messages []struct {
			Role    string          `json:"role"`
			Content json.RawMessage `json:"content"`
		} `json:"messages"`
	}
	if json.Unmarshal(body, &req) != nil {
		return nil
	}
	var out []string
	for i := len(req.Messages) - 1; i >= 0; i-- {
		m := req.Messages[i]
		if m.Role != "user" {
			continue
		}
		if text := messageText(m.Content); text != "" {
			out = append(out, text)
		}
	}
	slices.Reverse(out)
	return out
}

func systemText(body []byte) string {
	var req struct {
		Messages []struct {
			Role    string          `json:"role"`
			Content json.RawMessage `json:"content"`
		} `json:"messages"`
	}
	if json.Unmarshal(body, &req) != nil {
		return ""
	}
	for _, m := range req.Messages {
		if m.Role == "system" {
			return messageText(m.Content)
		}
	}
	return ""
}

func messageText(raw json.RawMessage) string {
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	var parts []struct {
		Text string `json:"text"`
	}
	if json.Unmarshal(raw, &parts) == nil {
		var out []string
		for _, p := range parts {
			out = append(out, p.Text)
		}
		return strings.Join(out, "\n")
	}
	return ""
}

// fakeProvider is a fake OpenAI-compatible provider that answers "echo: <last
// user text>" and records the user texts in arrival order.
type fakeProvider struct {
	*httptest.Server
	mu     sync.Mutex
	texts  []string
	bodies []string
	reply  func(body []byte, request int) string
}

func (p *fakeProvider) seen() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	return slices.Clone(p.texts)
}

// newFakeProvider starts the provider; delay, when not nil, returns how long
// to hold the response to a request with the given user text.
func newFakeProvider(t *testing.T, delay func(text string) time.Duration) *fakeProvider {
	return newScriptedFakeProvider(t, delay, nil)
}

func newScriptedFakeProvider(t *testing.T, delay func(text string) time.Duration, reply func(body []byte, request int) string) *fakeProvider {
	t.Helper()
	p := &fakeProvider{reply: reply}
	p.Server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/chat/completions" {
			http.NotFound(w, r)
			return
		}
		body, _ := io.ReadAll(r.Body)
		user := lastUserText(body)
		p.mu.Lock()
		request := len(p.texts)
		p.texts = append(p.texts, user)
		p.bodies = append(p.bodies, string(body))
		p.mu.Unlock()
		if delay != nil {
			time.Sleep(delay(user))
		}
		response := ""
		if p.reply != nil {
			response = p.reply(body, request)
		}
		if response == "" {
			response = chatTextReply("echo: " + user)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.Write([]byte(response))
	}))
	t.Cleanup(p.Close)
	return p
}

func chatTextReply(text string) string {
	content, _ := json.Marshal(text)
	return `data: {"choices":[{"delta":{"content":` + string(content) + `}}]}` + "\n\n" +
		`data: {"choices":[{"delta":{},"finish_reason":"stop"}]}` + "\n\n" + "data: [DONE]\n\n"
}

func chatToolCallReply(id, name, arguments string) string {
	chunk, _ := json.Marshal(map[string]any{"choices": []any{map[string]any{
		"delta": map[string]any{"tool_calls": []any{map[string]any{
			"index": 0, "id": id, "type": "function",
			"function": map[string]any{"name": name, "arguments": arguments},
		}}},
	}}})
	finish, _ := json.Marshal(map[string]any{"choices": []any{map[string]any{
		"delta": map[string]any{}, "finish_reason": "tool_calls",
	}}})
	return "data: " + string(chunk) + "\n\n" + "data: " + string(finish) + "\n\n" + "data: [DONE]\n\n"
}

// ottoHome returns a HOME whose otto config points at the provider.
func ottoHome(t *testing.T, providerURL string) string {
	t.Helper()
	home := t.TempDir()
	for _, d := range []string{"Library/Caches", ".config/otto"} {
		if err := os.MkdirAll(filepath.Join(home, d), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	cfg := "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"" + providerURL +
		"\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
	if err := os.WriteFile(filepath.Join(home, ".config/otto/config.toml"), []byte(cfg), 0o600); err != nil {
		t.Fatal(err)
	}
	return home
}

func ottoEnv(home string) []string {
	return []string{
		"HOME=" + home,
		"OTTO_API_KEY=sk-e2e-not-a-real-key",
		"PATH=/usr/bin:/bin:/usr/sbin:/sbin",
		"TMPDIR=" + os.Getenv("TMPDIR"),
	}
}

// ottoBin returns the absolute path of OTTO_BIN, or skips the test.
func ottoBin(t *testing.T) string {
	t.Helper()
	bin := os.Getenv("OTTO_BIN")
	if bin == "" {
		t.Skip("OTTO_BIN is not set")
	}
	if _, err := os.Stat(bin); err != nil {
		t.Fatalf("OTTO_BIN is set but unusable: %v", err)
	}
	bin, _ = filepath.Abs(bin)
	return bin
}

// TestEndToEndWithOtto runs the real `otto acp` against a fake
// OpenAI-compatible provider. Set OTTO_BIN to the otto binary to run it.
func TestEndToEndWithOtto(t *testing.T) {
	bin := ottoBin(t)

	provider := newFakeProvider(t, nil)
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}

	h := newHarness(t, setup{agent: &agent.Options{
		Command: []string{bin, "acp", "--sandbox", "off"},
		Dir:     workspace,
		Env:     ottoEnv(home),
	}})

	h.say("first question")
	h.say("second question")
	got := h.waitSent(2)
	if !slices.Equal(got, []string{"echo: first question", "echo: second question"}) {
		t.Fatalf("replies = %q", got)
	}
	sid := h.store.Session("fake:c1")
	if len(sid) != 32 {
		t.Fatalf("stored session id = %q", sid)
	}

	h.agent.Kill()
	h.say("third question")
	got = h.waitSent(3)
	settle()
	got = h.plat.texts()
	if !slices.Equal(got, []string{"echo: first question", "echo: second question", "echo: third question"}) {
		t.Fatalf("replies after restart = %q (replayed history must not reach the chat)", got)
	}
	if h.store.Session("fake:c1") != sid {
		t.Fatalf("session id changed from %s to %s: the session was not resumed with session/load", sid, h.store.Session("fake:c1"))
	}
}

// TestEndToEndSharedSession binds two chats to one session held by `otto
// serve`, through `otto acp --attach`, and checks that their turns run one
// after the other and that each chat receives only its own reply.
func TestEndToEndSharedSession(t *testing.T) {
	bin := ottoBin(t)
	provider := newFakeProvider(t, func(text string) time.Duration {
		if text == "from one" {
			return 700 * time.Millisecond
		}
		return 0
	})
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	sock, serveStderr := startOttoServer(t, bin, home, workspace)

	h := newHarness(t, setup{
		chats: []string{"c1", "c2"},
		agent: &agent.Options{
			Command: []string{bin, "acp", "--attach", "--socket", sock},
			Dir:     workspace,
			Env:     ottoEnv(home),
		},
	})
	say := func(chat, text string) { h.plat.deliver(h.msg(chat, "u1", text)) }
	repliesOf := func(chat string) []string {
		h.plat.mu.Lock()
		defer h.plat.mu.Unlock()
		var out []string
		for _, s := range h.plat.sent {
			if s.chat == chat {
				out = append(out, s.text)
			}
		}
		return out
	}

	say("c1", "setup")
	h.waitSent(1)
	sid := h.store.Session("fake:c1")
	if len(sid) != 32 {
		t.Fatalf("stored session id of c1 = %q; sent = %q; serve stderr:\n%s", sid, h.plat.texts(), serveStderr())
	}
	say("c2", "/use "+sid[:8])
	h.waitSent(2)
	if got := repliesOf("c2"); len(got) != 1 || !strings.HasPrefix(got[0], "Using session "+sid) {
		t.Fatalf("c2 reply to /use = %q", got)
	}
	if h.store.Session("fake:c2") != sid {
		t.Fatalf("c2 is bound to %q, want %q", h.store.Session("fake:c2"), sid)
	}

	say("c1", "from one") // the provider holds this response for 700 ms
	h.waitFor(func() bool { return slices.Contains(provider.seen(), "from one") }, "provider request from one")
	say("c2", "from two")
	h.waitSent(4)

	if got, want := repliesOf("c1"), []string{"echo: setup", "echo: from one"}; !slices.Equal(got, want) {
		t.Fatalf("c1 replies = %q, want %q", got, want)
	}
	if got, want := repliesOf("c2")[1:], []string{"echo: from two"}; !slices.Equal(got, want) {
		t.Fatalf("c2 replies = %q, want %q", got, want)
	}
	if got, want := provider.seen(), []string{"setup", "from one", "from two"}; !slices.Equal(got, want) {
		t.Fatalf("provider saw %q, want %q", got, want)
	}
	// c1's reply arrives before c2's: the second turn waits for the first.
	var order []string
	h.plat.mu.Lock()
	for _, s := range h.plat.sent[2:] {
		order = append(order, s.text)
	}
	h.plat.mu.Unlock()
	if !slices.Equal(order, []string{"echo: from one", "echo: from two"}) {
		t.Fatalf("reply order = %q", order)
	}
}

// The real ACP agent must turn remember's pending candidate into an actionable
// connector card; a tool-call notification alone is not a permission request.
func TestEndToEndMemoryApprovalCard(t *testing.T) {
	for _, tcp := range []bool{false, true} {
		t.Run(fmt.Sprintf("tcp=%t", tcp), func(t *testing.T) { testEndToEndMemoryApprovalCard(t, tcp) })
	}
}

func testEndToEndMemoryApprovalCard(t *testing.T, tcp bool) {
	bin := ottoBin(t)
	var calls int
	provider := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls++
		w.Header().Set("Content-Type", "text/event-stream")
		if calls == 1 {
			args, _ := json.Marshal(`{"kind":"preference","key":"tabs","text":"prefers tabs"}`)
			fmt.Fprintf(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-memory\",\"type\":\"function\",\"function\":{\"name\":\"remember\",\"arguments\":%s}}]},\"finish_reason\":null}]}\n\n", args)
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n")
		} else {
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"Queued.\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
		}
	}))
	defer provider.Close()
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	options := &agent.Options{Command: []string{bin, "acp", "--sandbox", "off"}, Dir: workspace, Env: ottoEnv(home)}
	if tcp {
		_, diagnostics := startOttoServer(t, bin, home, workspace, "--acp-listen", "127.0.0.1:0")
		options = tcpAgentOptions(t, diagnostics, workspace)
	}
	h := newHarness(t, setup{agent: options})
	h.say("Remember my preference for tabs.")
	waitCards(h, 1)
	if got := h.waitSent(2); !strings.Contains(got[1], "prefers tabs") {
		t.Fatal(got)
	}
	token := h.plat.approvalID()
	// Restart with the persisted chat binding. The pending card must return
	// without sending a new message or creating another approval identity.
	store := h.store
	if err := h.shutdown(); err != nil {
		t.Fatal(err)
	}
	h = newHarness(t, setup{agent: options, store: store})
	waitCards(h, 1)
	if h.plat.approvalID() != token {
		t.Fatal("restart replaced the durable approval identity")
	}
	h.agent.Kill() // a card must still work after ACP transport/process reconnect
	memoryClick(h, "c1", "u1", token, "/allow")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "real memory review")
	sid := h.store.Session("fake:c1")
	page, err := h.agent.MemoryPending(context.Background(), sid, "")
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Candidates) != 0 {
		t.Fatalf("accepted candidate is still pending: %+v", page)
	}
	h.plat.mu.Lock()
	defer h.plat.mu.Unlock()
	if h.plat.statuses[0] != "Approved." {
		t.Fatal(h.plat.statuses)
	}
}

// TestEndToEndApprovalDialogueRevokesPermission exercises cancellation through
// the real ACP SDK: the model withdraws a pending request, which must close its
// chat card promptly, and a late Allow click must not run the command.
func TestEndToEndApprovalDialogueRevokesPermission(t *testing.T) {
	for _, tcp := range []bool{false, true} {
		t.Run(fmt.Sprintf("tcp=%t", tcp), func(t *testing.T) { testEndToEndApprovalDialogueRevokesPermission(t, tcp) })
	}
}

func testEndToEndApprovalDialogueRevokesPermission(t *testing.T, tcp bool) {
	if runtime.GOOS != "darwin" {
		t.Skip("elevated bash approval requires macOS Seatbelt")
	}
	bin := ottoBin(t)
	requestID := regexp.MustCompile(`Only withdraw request ([^ ]+)\.`)
	provider := newScriptedFakeProvider(t, nil, func(body []byte, _ int) string {
		var req struct {
			Messages []struct {
				Role string `json:"role"`
			} `json:"messages"`
		}
		_ = json.Unmarshal(body, &req)
		hasToolResult := false
		for _, message := range req.Messages {
			hasToolResult = hasToolResult || message.Role == "tool"
		}
		if match := requestID.FindSubmatch(body); len(match) == 2 {
			if hasToolResult {
				return chatTextReply("The request was withdrawn.")
			}
			arguments, _ := json.Marshal(map[string]string{"id": string(match[1])})
			return chatToolCallReply("revoke-call", "approval_revoke", string(arguments))
		}
		if hasToolResult {
			return chatTextReply("approval needed")
		}
		arguments, _ := json.Marshal(map[string]string{
			"command":             `printf ran > "$HOME/otto-approval-ran"`,
			"sandbox_permissions": "require_escalated",
			"justification":       "create a marker only if this approval is granted",
		})
		return chatToolCallReply("approval-command", "bash", string(arguments))
	})
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	options := &agent.Options{Command: []string{bin, "acp", "--sandbox", "seatbelt"}, Dir: workspace, Env: ottoEnv(home)}
	if tcp {
		_, diagnostics := startOttoServer(t, bin, home, workspace, "--sandbox", "seatbelt", "--acp-listen", "127.0.0.1:0")
		options = tcpAgentOptions(t, diagnostics, workspace)
	}
	h := newHarness(t, setup{mod: func(o *Options) { o.PermissionTimeout = 2 * time.Minute }, agent: options})
	h.say("/auto off")
	h.waitSent(1)
	h.say("Run the command after I approve it.")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.approvals) == 1 }, "real Otto approval card")
	cardID := h.plat.approvalID()
	started := time.Now()
	h.say("Cancel that command")
	h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) == 1 }, "approval card withdrawal")
	if elapsed := time.Since(started); elapsed >= 5*time.Second {
		t.Fatalf("approval card took %s to close; it may have waited for expiry", elapsed)
	}
	h.plat.mu.Lock()
	status := h.plat.statuses[0]
	h.plat.mu.Unlock()
	if status != "Closed; answered elsewhere or cancelled." {
		t.Fatalf("approval card status = %q, want prompt cancellation", status)
	}
	lateAllow := h.msg("c1", "u1", "/allow")
	lateAllow.ApprovalID = cardID
	h.plat.deliver(lateAllow)
	h.waitFor(func() bool { return slices.Contains(h.plat.texts(), "This approval request is no longer pending.") }, "late approval rejection")
	if _, err := os.Stat(filepath.Join(home, "otto-approval-ran")); !os.IsNotExist(err) {
		t.Fatalf("withdrawn command left marker behind, stat error = %v", err)
	}
}

// startOttoServer runs a server owned by the test and returns its socket and diagnostics.
func startOttoServer(t *testing.T, bin, home, workspace string, extra ...string) (string, func() string) {
	t.Helper()
	// A short path: unix socket paths are limited to about 100 bytes.
	sockDir, err := os.MkdirTemp("", "otto-e2e")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(sockDir) })
	sock := filepath.Join(sockDir, "s")

	serve := exec.Command(bin, append([]string{"serve", "--socket", sock, "--cwd", workspace, "--sandbox", "off"}, extra...)...)
	serve.Dir = workspace
	serve.Env = append(ottoEnv(home), "OTTO_ACP_TOKEN=connect-test-acp-token")
	errFile, err := os.Create(filepath.Join(sockDir, "serve.err"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { errFile.Close() })
	serve.Stderr = errFile
	serve.Stdout = errFile
	serveStderr := func() string { b, _ := os.ReadFile(errFile.Name()); return string(b) }
	if err := serve.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = serve.Process.Kill()
		_ = serve.Wait()
	})
	deadline := time.Now().Add(20 * time.Second)
	for {
		c, err := net.Dial("unix", sock)
		if err == nil {
			c.Close()
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("otto serve did not listen on %s: %v; stderr:\n%s", sock, err, serveStderr())
		}
		time.Sleep(20 * time.Millisecond)
	}

	return sock, serveStderr
}

func TestEndToEndChatRoleAndChannel(t *testing.T) {
	bin := ottoBin(t)
	for _, mode := range []string{"local", "attach", "tcp"} {
		t.Run(mode, func(t *testing.T) {
			provider := newFakeProvider(t, nil)
			home := ottoHome(t, provider.URL)
			workspace, err := filepath.EvalSymlinks(t.TempDir())
			if err != nil {
				t.Fatal(err)
			}
			options := &agent.Options{Command: []string{bin, "acp", "--sandbox", "off"}, Dir: workspace, Env: ottoEnv(home)}
			if mode == "attach" {
				sock, _ := startOttoServer(t, bin, home, workspace)
				options = &agent.Options{Command: []string{bin, "acp", "--attach", "--socket", sock}, Dir: workspace, Env: ottoEnv(home)}
			}
			if mode == "tcp" {
				_, diagnostics := startOttoServer(t, bin, home, workspace, "--acp-listen", "127.0.0.1:0")
				options = tcpAgentOptions(t, diagnostics, workspace)
			}
			store, err := state.Open(filepath.Join(t.TempDir(), "state.json"))
			if err != nil {
				t.Fatal(err)
			}
			sid := ""
			for _, turn := range []struct {
				platform string
				text     string
				guidance string
			}{
				{"telegram", "help me plan my day", "The user is communicating through Telegram chat"},
				{"feishu", "write a short invitation", "The user is communicating through Feishu (Lark) chat"},
			} {
				h := newHarness(t, setup{platform: turn.platform, agent: &agent.Options{
					Command: options.Command, Address: options.Address, Token: options.Token, Dir: options.Dir, Env: options.Env,
				}, store: store})
				// Reuse the Telegram session from Feishu to check that the current
				// transport is supplied even when the session already has history.
				if sid != "" {
					if err := h.store.SetSession(turn.platform+":c1", sid); err != nil {
						t.Fatal(err)
					}
				}
				h.say(turn.text)
				if got := h.waitSent(1); got[0] != "echo: "+turn.text {
					t.Fatalf("reply = %q", got)
				}
				body := lastProviderBody(provider)
				if got := lastUserText([]byte(body)); got != turn.text {
					t.Fatalf("user text = %q, want %q", got, turn.text)
				}
				system := systemText([]byte(body))
				for _, want := range []string{
					"You are Otto, a general-purpose personal agent.",
					"For questions and discussion, answer directly.",
					turn.guidance,
					"does not grant additional tools or permissions",
				} {
					if !strings.Contains(system, want) {
						t.Fatalf("system prompt missing %q: %s", want, system)
					}
				}
				gotID := h.store.Session(turn.platform + ":c1")
				if sid != "" && gotID != sid {
					t.Fatalf("session changed: %q != %q", gotID, sid)
				}
				sid = gotID
				if err := h.shutdown(); err != nil {
					t.Fatal(err)
				}
			}
			plain := agent.New(*options)
			defer plain.Close()
			if err := plain.Load(context.Background(), sid); err != nil {
				t.Fatal(err)
			}
			const plainText = "non-chat follow-up"
			if _, err := plain.Prompt(context.Background(), sid, plainText); err != nil {
				t.Fatal(err)
			}
			body := lastProviderBody(provider)
			if got := userTexts([]byte(body)); !slices.Equal(got, []string{"help me plan my day", "write a short invitation", plainText}) {
				t.Fatalf("persisted user texts = %q", got)
			}
			system := systemText([]byte(body))
			if strings.Contains(system, "Telegram chat") || strings.Contains(system, "Feishu (Lark) chat") {
				t.Fatalf("non-chat turn retained channel context: %s", system)
			}
		})
	}
}

func lastProviderBody(provider *fakeProvider) string {
	provider.mu.Lock()
	defer provider.mu.Unlock()
	return provider.bodies[len(provider.bodies)-1]
}

// TCP drives serve directly: no ACP child, persistent shared sessions and a
// transport reconnect that leaves serve and its HTTP clients alive.
func TestEndToEndTCPSharedSession(t *testing.T) {
	bin := ottoBin(t)
	provider := newFakeProvider(t, nil)
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	sock, diagnostics := startOttoServer(t, bin, home, workspace, "--acp-listen", "127.0.0.1:0")
	address := tcpAgentOptions(t, diagnostics, workspace).Address
	options := func(token string) *agent.Options {
		return &agent.Options{Address: address, Token: token, Dir: workspace}
	}
	bad := agent.New(*options("wrong-token"))
	if _, err := bad.New(context.Background()); err == nil {
		t.Fatal("bad authentication admitted")
	}
	bad.Close()
	h := newHarness(t, setup{agent: options("connect-test-acp-token")})
	h.say("first over TCP")
	if got := h.waitSent(1); got[0] != "echo: first over TCP" {
		t.Fatal(got)
	}
	sid := h.store.Session("fake:c1")
	page, err := h.agent.MemoryPending(context.Background(), sid, "")
	if err != nil || len(page.Candidates) != 0 {
		t.Fatalf("TCP memory review unavailable: %v", err)
	}

	// The existing stdio relay accesses the same serve-owned session.
	local := agent.New(agent.Options{Command: []string{bin, "acp", "--attach", "--socket", sock}, Dir: workspace, Env: ottoEnv(home)})
	local.SetHandler(h.b)
	defer local.Close()
	if err := local.Load(context.Background(), sid); err != nil {
		t.Fatal(err)
	}
	if _, err := local.Prompt(context.Background(), sid, "shared from HTTP relay"); err != nil {
		t.Fatal(err)
	}
	h.agent.Kill() // disconnect transport only; the server must remain alive
	h.say("after TCP reconnect")
	if got := h.waitSent(2); got[1] != "echo: after TCP reconnect" {
		t.Fatalf("reconnect: %q; %s", got, diagnostics())
	}
	if h.store.Session("fake:c1") != sid {
		t.Fatal("reconnect changed the session")
	}
	if len(provider.seen()) != 3 {
		t.Fatal(provider.seen())
	}
	h.agent.Close()
	if _, err := local.List(context.Background()); err != nil {
		t.Fatalf("closing TCP stopped serve: %v", err)
	}
}

func tcpAgentOptions(t *testing.T, diagnostics func() string, workspace string) *agent.Options {
	t.Helper()
	match := regexp.MustCompile(`otto serve: ACP ([^\s]+)`).FindStringSubmatch(diagnostics())
	if len(match) != 2 {
		t.Fatalf("missing ACP address: %s", diagnostics())
	}
	return &agent.Options{Address: match[1], Token: "connect-test-acp-token", Dir: workspace}
}

func TestEndToEndModelInitiatedApproval(t *testing.T) {
	for _, tcp := range []bool{false, true} {
		t.Run(fmt.Sprintf("tcp=%t", tcp), func(t *testing.T) {
			bin := ottoBin(t)
			provider := newScriptedFakeProvider(t, nil, func(body []byte, _ int) string {
				if !strings.Contains(string(body), `"role":"tool"`) && !strings.Contains(string(body), `"role": "tool"`) {
					return chatToolCallReply("plan-call", "request_approval", `{"title":"Review parser scope","reason":"Confirm before changing code","proposal":"Change only parsing"}`)
				}
				return chatTextReply("The approval decision was received.")
			})
			defer provider.Close()
			home := ottoHome(t, provider.URL)
			workspace, err := filepath.EvalSymlinks(t.TempDir())
			if err != nil {
				t.Fatal(err)
			}
			options := &agent.Options{Command: []string{bin, "acp", "--sandbox", "off"}, Dir: workspace, Env: ottoEnv(home)}
			if tcp {
				_, diagnostics := startOttoServer(t, bin, home, workspace, "--acp-listen", "127.0.0.1:0")
				options = tcpAgentOptions(t, diagnostics, workspace)
			}
			h := newHarness(t, setup{agent: options})
			h.say("/auto off")
			h.waitSent(1)
			h.say("Ask me to approve the plan before continuing.")
			waitCards(h, 1)
			id := h.plat.approvalID()
			h.say("/allow " + id + " Keep the scope narrow")
			h.waitFor(func() bool { h.plat.mu.Lock(); defer h.plat.mu.Unlock(); return len(h.plat.statuses) > 0 }, "plan card decision")
			h.waitFor(func() bool {
				for _, text := range h.plat.texts() {
					if strings.Contains(text, "The approval decision was received.") {
						return true
					}
				}
				return false
			}, "plan continuation")
		})
	}
}

func TestEndToEndReflectionCommands(t *testing.T) {
	bin := ottoBin(t)
	for _, mode := range []string{"local", "attach", "tcp"} {
		t.Run(mode, func(t *testing.T) {
			provider := newScriptedFakeProvider(t, nil, func(body []byte, _ int) string {
				if strings.Contains(string(body), "You are a reflection assistant") {
					return chatTextReply(`{"memories":[],"skills":[]}`)
				}
				return ""
			})
			home := ottoHome(t, provider.URL)
			workspace, err := filepath.EvalSymlinks(t.TempDir())
			if err != nil {
				t.Fatal(err)
			}
			options := &agent.Options{Command: []string{bin, "acp", "--sandbox", "off"}, Dir: workspace, Env: ottoEnv(home)}
			if mode == "attach" {
				sock, _ := startOttoServer(t, bin, home, workspace)
				options.Command = []string{bin, "acp", "--attach", "--socket", sock}
			}
			if mode == "tcp" {
				_, diagnostics := startOttoServer(t, bin, home, workspace, "--acp-listen", "127.0.0.1:0")
				options = tcpAgentOptions(t, diagnostics, workspace)
			}
			h := newHarness(t, setup{platform: "telegram", agent: options})
			h.say("do useful work")
			h.waitSent(1)
			for i, command := range []string{"/skill", "/skill generated", "/reflect"} {
				h.say(command)
				got := h.waitSent(i + 2)[i+1]
				want := []string{"Usage: /skill generated", "No skills generated", "reflection:"}[i]
				if !strings.Contains(got, want) {
					t.Fatalf("%s: reply = %q, want %q", command, got, want)
				}
			}
			provider.mu.Lock()
			bodies := slices.Clone(provider.bodies)
			provider.mu.Unlock()
			if len(bodies) != 2 || !strings.Contains(bodies[1], "You are a reflection assistant") {
				t.Fatalf("commands did not use the reflection pipeline: %v", bodies)
			}
			if err := h.shutdown(); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestEndToEndCompletedWorkAutomaticallyReflects(t *testing.T) {
	bin := ottoBin(t)
	provider := newScriptedFakeProvider(t, nil, func(body []byte, request int) string {
		if strings.Contains(string(body), "You are a reflection assistant") {
			return chatTextReply(`{"memories":[],"skills":[]}`)
		}
		if request < 10 {
			// A distinct limit per call keeps the no-progress guard, which
			// stops a turn after 8 identical calls in a row, from ending it.
			arguments := fmt.Sprintf(`{"path":"notes.txt","limit":%d}`, request+1)
			return chatToolCallReply(fmt.Sprintf("read-%d", request), "read", arguments)
		}
		return chatTextReply("work complete")
	})
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(workspace, "notes.txt"), []byte("verified local procedure\n"), 0600); err != nil {
		t.Fatal(err)
	}
	_, diagnostics := startOttoServer(t, bin, home, workspace, "--acp-listen", "127.0.0.1:0")
	h := newHarness(t, setup{platform: "telegram", agent: tcpAgentOptions(t, diagnostics, workspace)})
	h.say("complete a complex local task")
	if got := h.waitSent(1)[0]; got != "work complete" {
		t.Fatalf("task reply = %q", got)
	}
	deadline := time.Now().Add(10 * time.Second)
	for {
		provider.mu.Lock()
		bodies := slices.Clone(provider.bodies)
		provider.mu.Unlock()
		if len(bodies) == 12 && strings.Contains(bodies[11], "You are a reflection assistant") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("successful work did not automatically reflect: %d requests", len(bodies))
		}
		time.Sleep(20 * time.Millisecond)
	}
	if err := h.shutdown(); err != nil {
		t.Fatal(err)
	}
}
