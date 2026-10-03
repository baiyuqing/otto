package bridge

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/connect/internal/agent"
	"github.com/baiyuqing/otto/connect/internal/agent/fake"
	"github.com/baiyuqing/otto/connect/internal/manage"
	"github.com/baiyuqing/otto/connect/internal/state"
)

func TestMain(m *testing.M) {
	if fake.Main() {
		return
	}
	os.Exit(m.Run())
}

type sent struct {
	chat, replyTo, text string
	markdown            bool
}

// fakePlatform records what the bridge sends and lets the test deliver
// messages.
type fakePlatform struct {
	name      string
	mu        sync.Mutex
	deliver   func(Message)
	started   chan struct{}
	sent      []sent
	typing    int
	approvals []string
	statuses  []string
	cardErr   error
}

func newFakePlatform() *fakePlatform { return &fakePlatform{started: make(chan struct{})} }

// managementServer supplies an HTTP client to the bridge. Production creates
// the same API client from the attach command's Unix socket; this test helper
// uses httptest because the sandbox does not allow listener binds on Unix paths.
func managementServer(t *testing.T, handler http.HandlerFunc) *manage.Client {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	return manage.NewWithBaseURL(server.Client(), server.URL)
}

func (p *fakePlatform) Name() string {
	if p.name != "" {
		return p.name
	}
	return "fake"
}

func (p *fakePlatform) Run(ctx context.Context, deliver func(Message)) error {
	p.deliver = deliver
	close(p.started)
	<-ctx.Done()
	return nil
}

func (p *fakePlatform) Send(_ context.Context, chat, replyTo, text string, markdown bool) error {
	p.mu.Lock()
	p.sent = append(p.sent, sent{chat, replyTo, text, markdown})
	p.mu.Unlock()
	return nil
}

func (p *fakePlatform) Typing(context.Context, string) error {
	p.mu.Lock()
	p.typing++
	p.mu.Unlock()
	return nil
}

func (p *fakePlatform) texts() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	var out []string
	for _, s := range p.sent {
		out = append(out, s.text)
	}
	return out
}

type harness struct {
	t     *testing.T
	dir   string // fake agent state
	store *state.Store
	plat  *fakePlatform
	b     *Bridge
	agent *agent.Agent
	done  chan error
	stop  context.CancelFunc
	n     int
}

type textPlatform struct{ Platform }

type setup struct {
	platform string
	textOnly bool
	dir      string // fake agent state; default a new temp dir
	store    *state.Store
	env      []string // extra agent environment
	chats    []string
	senders  []string
	mod      func(*Options)
	manage   *manage.Client
	agent    *agent.Options // replaces the fake agent's options
}

func newHarness(t *testing.T, s setup) *harness {
	t.Helper()
	if s.dir == "" {
		s.dir = t.TempDir()
	}
	if s.store == nil {
		var err error
		if s.store, err = state.Open(filepath.Join(t.TempDir(), "state.json")); err != nil {
			t.Fatal(err)
		}
	}
	if s.chats == nil {
		s.chats = []string{"c1"}
	}
	if s.senders == nil {
		s.senders = []string{"u1"}
	}
	h := &harness{t: t, dir: s.dir, store: s.store, plat: newFakePlatform(), done: make(chan error, 1)}
	h.plat.name = s.platform
	ao := agent.Options{
		Command: []string{os.Args[0]},
		Dir:     s.dir,
		Env:     fake.Env(s.dir, s.env...),
		Sleep:   func(context.Context, time.Duration) error { return nil },
	}
	if s.agent != nil {
		ao = *s.agent
	}
	h.agent = agent.New(ao)
	opts := Options{
		Agent:          h.agent,
		Manage:         s.manage,
		Platforms:      []Platform{h.plat},
		Access:         map[string]Access{h.plat.Name(): {Chats: s.chats, Senders: s.senders}},
		Store:          s.store,
		TypingInterval: time.Hour,
	}
	if s.textOnly {
		opts.Platforms = []Platform{textPlatform{h.plat}}
	}
	if s.mod != nil {
		s.mod(&opts)
	}
	h.b = New(opts)
	ctx, cancel := context.WithCancel(context.Background())
	h.stop = cancel
	go func() { h.done <- h.b.Run(ctx) }()
	<-h.plat.started
	t.Cleanup(func() { h.shutdown() })
	return h
}

// shutdown ends Run and returns its error.
func (h *harness) shutdown() error {
	h.stop()
	select {
	case err := <-h.done:
		h.done <- err
		return err
	case <-time.After(30 * time.Second):
		h.t.Fatal("Run did not return after shutdown")
		return nil
	}
}

func (h *harness) msg(chat, sender, text string) Message {
	h.n++
	return Message{Platform: h.plat.Name(), ChatID: chat, SenderID: sender, MessageID: "m" + string(rune('0'+h.n%10)), Text: text, Time: time.Now()}
}

// say delivers text from the admitted chat and sender.
func (h *harness) say(text string) { h.plat.deliver(h.msg("c1", "u1", text)) }

// waitSent waits until n messages have been sent and returns their texts.
func (h *harness) waitSent(n int) []string {
	h.t.Helper()
	return h.waitFor(func() bool { return len(h.plat.texts()) >= n }, "sent messages")
}

func (h *harness) waitFor(cond func() bool, what string) []string {
	h.t.Helper()
	deadline := time.Now().Add(20 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			h.t.Fatalf("timed out waiting for %s; sent = %q, agent calls = %q", what, h.plat.texts(), fake.Calls(h.dir))
		}
		time.Sleep(5 * time.Millisecond)
	}
	return h.plat.texts()
}

// waitCall waits until the fake agent logged a line with the prefix and
// returns the line.
func (h *harness) waitCall(prefix string) string {
	h.t.Helper()
	var found string
	h.waitFor(func() bool {
		for _, c := range fake.Calls(h.dir) {
			if strings.HasPrefix(c, prefix) {
				found = c
				return true
			}
		}
		return false
	}, "agent call "+prefix)
	return found
}

func (h *harness) calls(prefix string) []string {
	var out []string
	for _, c := range fake.Calls(h.dir) {
		if strings.HasPrefix(c, prefix) {
			out = append(out, c)
		}
	}
	return out
}

// settle gives the bridge time to (wrongly) act on something it should
// ignore.
func settle() { time.Sleep(150 * time.Millisecond) }

func newStore(t *testing.T) *state.Store {
	t.Helper()
	s, err := state.Open(filepath.Join(t.TempDir(), "state.json"))
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func (p *fakePlatform) SendApproval(ctx context.Context, chat, id, text string) (func(context.Context, string) error, error) {
	p.mu.Lock()
	p.approvals = append(p.approvals, id)
	err := p.cardErr
	p.mu.Unlock()
	if err != nil {
		return nil, err
	}
	p.Send(ctx, chat, "", text, false)
	return func(_ context.Context, status string) error {
		p.mu.Lock()
		defer p.mu.Unlock()
		p.statuses = append(p.statuses, status)
		return nil
	}, nil
}
func (p *fakePlatform) approvalID() string {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.approvals[len(p.approvals)-1]
}
