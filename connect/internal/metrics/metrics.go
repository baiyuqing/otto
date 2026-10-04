// Package metrics counts connector events and serves them in Prometheus text
// format 0.0.4 on a loopback listener. No metric is labelled by chat, sender,
// session or request id.
package metrics

import (
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"runtime/debug"
	"sort"
	"strings"
	"sync"
	"time"
)

var buckets = []float64{0.5, 1, 2.5, 5, 10, 30, 60, 120, 300, 600}

type hist struct {
	counts []uint64 // per bucket, not cumulative
	n      uint64
	sum    float64
}

// Metrics is safe for concurrent use. Every method accepts a nil receiver and
// does nothing, so components work without a registry.
type Metrics struct {
	mu       sync.Mutex
	messages map[[2]string]uint64 // platform, outcome
	turns    map[[2]string]uint64 // platform, stop_reason
	replies  map[[2]string]uint64 // platform, outcome
	durs     map[string]*hist     // platform
	restarts uint64
	agentUp  int
	version  string
}

// New returns a Metrics whose build_info carries the module version of the
// binary, plus the short VCS revision when present.
func New() *Metrics {
	return &Metrics{
		messages: map[[2]string]uint64{}, turns: map[[2]string]uint64{},
		replies: map[[2]string]uint64{}, durs: map[string]*hist{},
		version: buildVersion(),
	}
}

func buildVersion() string {
	bi, ok := debug.ReadBuildInfo()
	if !ok {
		return "unknown"
	}
	v := bi.Main.Version
	if v == "" {
		v = "unknown"
	}
	for _, s := range bi.Settings {
		if s.Key == "vcs.revision" && s.Value != "" {
			v += "+" + s.Value[:min(len(s.Value), 7)]
		}
	}
	return v
}

// Message counts an inbound message: outcome is accepted, rejected or queue_full.
func (m *Metrics) Message(platform, outcome string) {
	m.inc(func() { m.messages[[2]string{platform, outcome}]++ })
}

// Reply counts a reply send: outcome is ok or error.
func (m *Metrics) Reply(platform, outcome string) {
	m.inc(func() { m.replies[[2]string{platform, outcome}]++ })
}

// Turn records a finished turn; stopReason is the ACP stop reason or "error".
func (m *Metrics) Turn(platform, stopReason string, d time.Duration) {
	m.inc(func() {
		m.turns[[2]string{platform, stopReason}]++
		h := m.durs[platform]
		if h == nil {
			h = &hist{counts: make([]uint64, len(buckets))}
			m.durs[platform] = h
		}
		s := d.Seconds()
		h.n++
		h.sum += s
		for i, b := range buckets {
			if s <= b {
				h.counts[i]++
				break
			}
		}
	})
}

// AgentRestart counts a restart of the agent connection.
func (m *Metrics) AgentRestart() { m.inc(func() { m.restarts++ }) }

// AgentUp adds delta (+1 on connect, -1 on exit) to the live agent count.
func (m *Metrics) AgentUp(delta int) { m.inc(func() { m.agentUp += delta }) }

func (m *Metrics) inc(f func()) {
	if m == nil {
		return
	}
	m.mu.Lock()
	f()
	m.mu.Unlock()
}

func esc(s string) string {
	return strings.NewReplacer(`\`, `\\`, `"`, `\"`, "\n", `\n`).Replace(s)
}

func sortedKeys(m map[[2]string]uint64) [][2]string {
	keys := make([][2]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool {
		if keys[i][0] != keys[j][0] {
			return keys[i][0] < keys[j][0]
		}
		return keys[i][1] < keys[j][1]
	})
	return keys
}

// Render writes every family in text format 0.0.4.
func (m *Metrics) Render(w io.Writer) {
	m.mu.Lock()
	defer m.mu.Unlock()
	fmt.Fprint(w, "# HELP otto_connect_build_info Connector build; always 1.\n# TYPE otto_connect_build_info gauge\n")
	fmt.Fprintf(w, "otto_connect_build_info{version=\"%s\"} 1\n", esc(m.version))
	counter := func(name, help, l2 string, v map[[2]string]uint64) {
		fmt.Fprintf(w, "# HELP %s %s\n# TYPE %s counter\n", name, help, name)
		for _, k := range sortedKeys(v) {
			fmt.Fprintf(w, "%s{platform=\"%s\",%s=\"%s\"} %d\n", name, esc(k[0]), l2, esc(k[1]), v[k])
		}
	}
	counter("otto_connect_messages_total", "Inbound messages by outcome (accepted, rejected, queue_full).", "outcome", m.messages)
	counter("otto_connect_turns_total", "Finished turns by ACP stop reason, or error.", "stop_reason", m.turns)
	const dur = "otto_connect_turn_duration_seconds"
	fmt.Fprintf(w, "# HELP %s Turn duration from prompt to stop reason.\n# TYPE %s histogram\n", dur, dur)
	plats := make([]string, 0, len(m.durs))
	for p := range m.durs {
		plats = append(plats, p)
	}
	sort.Strings(plats)
	for _, p := range plats {
		h := m.durs[p]
		var cum uint64
		for i, b := range buckets {
			cum += h.counts[i]
			fmt.Fprintf(w, "%s_bucket{platform=\"%s\",le=\"%g\"} %d\n", dur, esc(p), b, cum)
		}
		fmt.Fprintf(w, "%s_bucket{platform=\"%s\",le=\"+Inf\"} %d\n%s_sum{platform=\"%s\"} %g\n%s_count{platform=\"%s\"} %d\n",
			dur, esc(p), h.n, dur, esc(p), h.sum, dur, esc(p), h.n)
	}
	counter("otto_connect_replies_total", "Reply sends by outcome (ok, error).", "outcome", m.replies)
	fmt.Fprintf(w, "# HELP otto_connect_agent_restarts_total Agent process restarts.\n# TYPE otto_connect_agent_restarts_total counter\notto_connect_agent_restarts_total %d\n", m.restarts)
	up := 0
	if m.agentUp > 0 {
		up = 1
	}
	fmt.Fprintf(w, "# HELP otto_connect_agent_up 1 while the agent connection is up.\n# TYPE otto_connect_agent_up gauge\notto_connect_agent_up %d\n", up)
}

// Handler serves GET /metrics and GET /healthz.
func (m *Metrics) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /metrics", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
		m.Render(w)
	})
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		io.WriteString(w, `{"status":"ok"}`)
	})
	return mux
}

// Serve listens on addr and serves until ctx ends. It returns the bind error
// at once; the caller decides whether that is fatal.
func (m *Metrics) Serve(ctx context.Context, addr string) error {
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return err
	}
	srv := &http.Server{Handler: m.Handler(), ReadHeaderTimeout: 10 * time.Second}
	go func() {
		<-ctx.Done()
		sc, cancel := context.WithTimeout(context.Background(), time.Second)
		defer cancel()
		_ = srv.Shutdown(sc)
	}()
	go func() {
		_ = srv.Serve(ln) // returns http.ErrServerClosed at shutdown
	}()
	return nil
}
