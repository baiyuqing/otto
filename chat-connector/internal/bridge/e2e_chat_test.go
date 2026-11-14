package bridge

import (
	"context"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/baiyuqing/otto/chat-connector/internal/state"
)

func TestEndToEndChatRoleAndChannel(t *testing.T) {
	bin := ottoBin(t)
	for _, mode := range []string{"local", "attach"} {
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
			store, err := state.Open(filepath.Join(t.TempDir(), "state.json"))
			if err != nil {
				t.Fatal(err)
			}
			sid := ""
			for turnIndex, turn := range []struct {
				platform string
				text     string
				guidance string
			}{
				{"telegram", "help me plan my day", "The user is communicating through Telegram chat"},
				{"telegram", "write a short invitation", "The user is communicating through Telegram chat"},
			} {
				h := newHarness(t, setup{platform: turn.platform, agent: &agent.Options{
					Command: options.Command, Dir: options.Dir, Env: options.Env,
				}, store: store})
				// Reuse the first turn's session to check that the current
				// transport is supplied even when the session already has history.
				if sid != "" {
					if err := h.store.SetSession(turn.platform+":c1", sid); err != nil {
						t.Fatal(err)
					}
				}
				// Each new harness resets its local message counter. Distinct messages
				// in one persisted session need distinct platform IDs.
				message := h.msg("c1", "u1", turn.text)
				message.MessageID = fmt.Sprintf("m%d", turnIndex+1)
				if got, want := chatInputID(message), fmt.Sprintf("chat:telegram:c1:m%d", turnIndex+1); got != want {
					t.Fatalf("input id = %q, want %q", got, want)
				}
				h.plat.deliver(message)
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
			var actualUserTexts []string
			for _, text := range userTexts([]byte(body)) {
				// Runtime provenance is represented as a separate user-role message.
				if !strings.HasPrefix(text, "Runtime origin for the following user input ") {
					actualUserTexts = append(actualUserTexts, text)
				}
			}
			if !slices.Equal(actualUserTexts, []string{"help me plan my day", "write a short invitation", plainText}) {
				t.Fatalf("persisted user texts = %q", actualUserTexts)
			}
			system := systemText([]byte(body))
			if strings.Contains(system, "Telegram chat") {
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

// Attach mode: persistent shared sessions and a transport reconnect that
// leaves serve and its HTTP clients alive.
func TestEndToEndAttachSharedSession(t *testing.T) {
	bin := ottoBin(t)
	provider := newFakeProvider(t, nil)
	home := ottoHome(t, provider.URL)
	workspace, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	sock, diagnostics := startOttoServer(t, bin, home, workspace)
	attach := agent.Options{Command: []string{bin, "acp", "--attach", "--socket", sock}, Dir: workspace, Env: ottoEnv(home)}
	opts := attach
	h := newHarness(t, setup{agent: &opts})
	h.say("first over attach")
	if got := h.waitSent(1); got[0] != "echo: first over attach" {
		t.Fatal(got)
	}
	sid := h.store.Session("fake:c1")
	page, err := h.agent.MemoryPending(context.Background(), sid, "")
	if err != nil || len(page.Candidates) != 0 {
		t.Fatalf("attach memory review unavailable: %v", err)
	}

	// A second stdio relay accesses the same serve-owned session.
	local := agent.New(attach)
	local.SetHandler(h.b)
	defer local.Close()
	if err := local.Load(context.Background(), sid); err != nil {
		t.Fatal(err)
	}
	if _, err := local.Prompt(context.Background(), sid, "shared from HTTP relay"); err != nil {
		t.Fatal(err)
	}
	h.agent.Kill() // stop this relay only; the server must remain alive
	h.say("after attach reconnect")
	if got := h.waitSent(2); got[1] != "echo: after attach reconnect" {
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
		t.Fatalf("closing the first relay stopped serve: %v", err)
	}
}

func TestEndToEndModelInitiatedApproval(t *testing.T) {
	for _, attach := range []bool{false, true} {
		t.Run(fmt.Sprintf("attach=%t", attach), func(t *testing.T) {
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
			if attach {
				sock, _ := startOttoServer(t, bin, home, workspace)
				options = &agent.Options{Command: []string{bin, "acp", "--attach", "--socket", sock}, Dir: workspace, Env: ottoEnv(home)}
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
