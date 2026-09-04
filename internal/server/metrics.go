package server

import (
	"fmt"
	"net/http"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
)

var httpBuckets = []float64{0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10}
var turnToolBuckets = []float64{0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 120, 300, 600}

type histogram struct {
	buckets   []float64
	binCounts []uint64
	sum       float64
	count     uint64
}

func newHistogram(buckets []float64) *histogram {
	return &histogram{buckets: buckets, binCounts: make([]uint64, len(buckets))}
}

func (h *histogram) observe(v float64) {
	h.sum += v
	h.count++
	idx := sort.SearchFloat64s(h.buckets, v)
	if idx < len(h.buckets) {
		h.binCounts[idx]++
	}
}

type httpKey struct{ route, method, status string }
type toolKey struct{ tool, status string }

type metrics struct {
	mu sync.Mutex

	httpTotal    map[httpKey]uint64
	httpDuration map[string]*histogram

	sessionsOpenValue int64
	turnsActiveValue  int64

	turnsTotal   map[string]uint64
	turnDuration *histogram

	toolCallsTotal map[toolKey]uint64
	toolDuration   map[string]*histogram

	tokensTotal map[string]uint64

	streamClientsValue int64

	tasksStartedValue  uint64
	tasksFinishedTotal map[string]uint64
	tasksRunningValue  int64
}

func newMetrics() *metrics {
	return &metrics{
		httpTotal:          make(map[httpKey]uint64),
		httpDuration:       make(map[string]*histogram),
		turnsTotal:         make(map[string]uint64),
		turnDuration:       newHistogram(turnToolBuckets),
		toolCallsTotal:     make(map[toolKey]uint64),
		toolDuration:       make(map[string]*histogram),
		tokensTotal:        make(map[string]uint64),
		tasksFinishedTotal: make(map[string]uint64),
	}
}

func (m *metrics) httpRequest(route, method string, status int, d time.Duration) {
	key := httpKey{route: route, method: method, status: strconv.Itoa(status)}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.httpTotal[key]++
	h, ok := m.httpDuration[route]
	if !ok {
		h = newHistogram(httpBuckets)
		m.httpDuration[route] = h
	}
	h.observe(d.Seconds())
}

func (m *metrics) sessionsOpen(delta int) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.sessionsOpenValue += int64(delta)
}

func (m *metrics) turnStarted() {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.turnsActiveValue++
}

func (m *metrics) turnFinished(status string, d time.Duration) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.turnsActiveValue--
	m.turnsTotal[status]++
	m.turnDuration.observe(d.Seconds())
}

func (m *metrics) toolCall(tool string, isError bool, d time.Duration) {
	status := "ok"
	if isError {
		status = "error"
	}
	key := toolKey{tool: tool, status: status}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.toolCallsTotal[key]++
	h, ok := m.toolDuration[tool]
	if !ok {
		h = newHistogram(turnToolBuckets)
		m.toolDuration[tool] = h
	}
	h.observe(d.Seconds())
}

func (m *metrics) tokens(usage model.Usage) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if usage.InputTokens != 0 {
		m.tokensTotal["input"] += uint64(usage.InputTokens)
	}
	if usage.OutputTokens != 0 {
		m.tokensTotal["output"] += uint64(usage.OutputTokens)
	}
	if usage.CachedInputTokens != 0 {
		m.tokensTotal["cached_input"] += uint64(usage.CachedInputTokens)
	}
}

func (m *metrics) streamClients(delta int) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.streamClientsValue += int64(delta)
}

func (m *metrics) taskStarted() {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.tasksStartedValue++
}

func (m *metrics) taskFinished(status string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.tasksFinishedTotal[status]++
}

func (m *metrics) tasksRunning(delta int) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.tasksRunningValue += int64(delta)
}

// diffTasks compares list against seen (each task id's last-observed
// status, updated in place) and adjusts the task counters/gauge for every
// change: an id absent from seen counts one started; a task whose status is
// newly final counts one finished{status}; the running gauge changes by the
// number of tasks whose status became, or stopped being, exactly
// agent.TaskRunning. An id that is both unseen and already final counts as
// both started and finished, since Tasks().Updates() has capacity 1 and
// coalesces signals, so a fast task can skip past being observed as running.
func (m *metrics) diffTasks(seen map[string]agent.TaskStatus, list []agent.Task) {
	for _, task := range list {
		prev, existed := seen[task.ID]
		wasRunning := existed && prev == agent.TaskRunning
		wasFinal := existed && (agent.Task{Status: prev}).Final()

		if !existed {
			m.taskStarted()
		}
		if task.Status == agent.TaskRunning && !wasRunning {
			m.tasksRunning(1)
		} else if wasRunning && task.Status != agent.TaskRunning {
			m.tasksRunning(-1)
		}
		if task.Final() && !wasFinal {
			m.taskFinished(string(task.Status))
		}
		seen[task.ID] = task.Status
	}
}

func (m *metrics) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")

	m.mu.Lock()
	var b strings.Builder
	writeCounterHTTPRequests(&b, m.httpTotal)
	writeHistogramByLabel(&b, "otto_http_request_duration_seconds", "route", "HTTP request duration in seconds.", m.httpDuration)
	writeGauge(&b, "otto_sessions_open", "Number of sessions currently open.", m.sessionsOpenValue)
	writeCounterByLabel(&b, "otto_turns_total", "status", "Total turns by terminal status.", m.turnsTotal)
	writeGauge(&b, "otto_turns_active", "Number of turns currently active.", m.turnsActiveValue)
	writeSingletonHistogram(&b, "otto_turn_duration_seconds", "Turn duration in seconds.", m.turnDuration)
	writeCounterToolCalls(&b, m.toolCallsTotal)
	writeHistogramByLabel(&b, "otto_tool_call_duration_seconds", "tool", "Tool call duration in seconds.", m.toolDuration)
	writeCounterByLabel(&b, "otto_provider_tokens_total", "kind", "Total provider tokens by kind.", m.tokensTotal)
	writeGauge(&b, "otto_event_stream_clients", "Number of connected event stream clients.", m.streamClientsValue)
	writeCounter(&b, "otto_tasks_started_total", "Total sub-agent tasks started.", m.tasksStartedValue)
	writeCounterByLabel(&b, "otto_tasks_finished_total", "status", "Total sub-agent tasks finished by status.", m.tasksFinishedTotal)
	writeGauge(&b, "otto_tasks_running", "Number of sub-agent tasks currently running.", m.tasksRunningValue)
	m.mu.Unlock()

	_, _ = w.Write([]byte(b.String()))
}

func writeHelp(b *strings.Builder, name, mtype, help string) {
	fmt.Fprintf(b, "# HELP %s %s\n", name, help)
	fmt.Fprintf(b, "# TYPE %s %s\n", name, mtype)
}

func escapeLabelValue(v string) string {
	v = strings.ReplaceAll(v, `\`, `\\`)
	v = strings.ReplaceAll(v, `"`, `\"`)
	v = strings.ReplaceAll(v, "\n", `\n`)
	return v
}

func quoteLabel(v string) string {
	return `"` + escapeLabelValue(v) + `"`
}

func formatFloat(v float64) string {
	return strconv.FormatFloat(v, 'g', -1, 64)
}

func writeGauge(b *strings.Builder, name, help string, v int64) {
	writeHelp(b, name, "gauge", help)
	fmt.Fprintf(b, "%s %d\n", name, v)
}

func writeCounter(b *strings.Builder, name, help string, v uint64) {
	writeHelp(b, name, "counter", help)
	fmt.Fprintf(b, "%s %d\n", name, v)
}

func writeCounterByLabel(b *strings.Builder, name, labelName, help string, data map[string]uint64) {
	writeHelp(b, name, "counter", help)
	keys := make([]string, 0, len(data))
	for k := range data {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		fmt.Fprintf(b, "%s{%s=%s} %d\n", name, labelName, quoteLabel(k), data[k])
	}
}

func writeCounterHTTPRequests(b *strings.Builder, data map[httpKey]uint64) {
	const name = "otto_http_requests_total"
	writeHelp(b, name, "counter", "Total HTTP requests.")
	keys := make([]httpKey, 0, len(data))
	for k := range data {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool {
		if keys[i].route != keys[j].route {
			return keys[i].route < keys[j].route
		}
		if keys[i].method != keys[j].method {
			return keys[i].method < keys[j].method
		}
		return keys[i].status < keys[j].status
	})
	for _, k := range keys {
		fmt.Fprintf(b, "%s{route=%s,method=%s,status=%s} %d\n", name, quoteLabel(k.route), quoteLabel(k.method), quoteLabel(k.status), data[k])
	}
}

func writeCounterToolCalls(b *strings.Builder, data map[toolKey]uint64) {
	const name = "otto_tool_calls_total"
	writeHelp(b, name, "counter", "Total tool calls by tool and status.")
	keys := make([]toolKey, 0, len(data))
	for k := range data {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool {
		if keys[i].tool != keys[j].tool {
			return keys[i].tool < keys[j].tool
		}
		return keys[i].status < keys[j].status
	})
	for _, k := range keys {
		fmt.Fprintf(b, "%s{tool=%s,status=%s} %d\n", name, quoteLabel(k.tool), quoteLabel(k.status), data[k])
	}
}

func writeHistogramSamples(b *strings.Builder, name, labels string, h *histogram) {
	cum := uint64(0)
	for i, upper := range h.buckets {
		cum += h.binCounts[i]
		fmt.Fprintf(b, "%s_bucket{%sle=%s} %d\n", name, labels, quoteLabel(formatFloat(upper)), cum)
	}
	fmt.Fprintf(b, "%s_bucket{%sle=\"+Inf\"} %d\n", name, labels, h.count)
	fmt.Fprintf(b, "%s_sum{%s} %s\n", name, strings.TrimSuffix(labels, ","), formatFloat(h.sum))
	fmt.Fprintf(b, "%s_count{%s} %d\n", name, strings.TrimSuffix(labels, ","), h.count)
}

func writeSingletonHistogram(b *strings.Builder, name, help string, h *histogram) {
	writeHelp(b, name, "histogram", help)
	if h.count == 0 {
		return
	}
	cum := uint64(0)
	for i, upper := range h.buckets {
		cum += h.binCounts[i]
		fmt.Fprintf(b, "%s_bucket{le=%s} %d\n", name, quoteLabel(formatFloat(upper)), cum)
	}
	fmt.Fprintf(b, "%s_bucket{le=\"+Inf\"} %d\n", name, h.count)
	fmt.Fprintf(b, "%s_sum %s\n", name, formatFloat(h.sum))
	fmt.Fprintf(b, "%s_count %d\n", name, h.count)
}

func writeHistogramByLabel(b *strings.Builder, name, labelName, help string, data map[string]*histogram) {
	writeHelp(b, name, "histogram", help)
	keys := make([]string, 0, len(data))
	for k := range data {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		writeHistogramSamples(b, name, labelName+"="+quoteLabel(k)+",", data[k])
	}
}
