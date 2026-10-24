package bridge

import (
	"context"
	"fmt"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/baiyuqing/otto/chat-connector/internal/state"
)

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
			h := newHarness(t, setup{platform: "telegram", agent: options})
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
