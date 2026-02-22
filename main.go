package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"math"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

type config struct {
	dsn            string
	query          string
	concurrency    int
	duration       time.Duration
	reportInterval time.Duration
	mysqlBin       string
}

type mysqlTarget struct {
	user string
	pass string
	host string
	port string
	db   string
}

type metrics struct {
	totalSuccess atomic.Uint64
	totalFailure atomic.Uint64
	windowCount  atomic.Uint64
	windowErrors atomic.Uint64

	mu            sync.Mutex
	windowLatency []float64
	totalLatency  []float64
	startedAt     time.Time
}

func parseDSN(dsn string) (mysqlTarget, error) {
	// expected: user:pass@tcp(host:port)/db
	var t mysqlTarget
	at := strings.Index(dsn, "@tcp(")
	if at <= 0 {
		return t, errors.New("dsn must look like user:pass@tcp(host:port)/db")
	}
	creds := dsn[:at]
	rest := dsn[at+5:]

	credsParts := strings.SplitN(creds, ":", 2)
	if len(credsParts) != 2 || credsParts[0] == "" {
		return t, errors.New("dsn credentials must be user:pass")
	}
	t.user = credsParts[0]
	t.pass = credsParts[1]

	closeIdx := strings.Index(rest, ")/")
	if closeIdx <= 0 {
		return t, errors.New("dsn missing )/ after host:port")
	}
	hostPort := rest[:closeIdx]
	after := rest[closeIdx+2:]
	if hostPort == "" || after == "" {
		return t, errors.New("dsn must include host:port and db")
	}
	h, p, err := net.SplitHostPort(hostPort)
	if err != nil {
		return t, fmt.Errorf("invalid host:port %q: %w", hostPort, err)
	}
	t.host = h
	t.port = p
	t.db = strings.SplitN(after, "?", 2)[0]
	if t.db == "" {
		return t, errors.New("dsn database name is empty")
	}
	return t, nil
}

func (m *metrics) record(duration time.Duration, err error) {
	if err != nil {
		m.totalFailure.Add(1)
		m.windowErrors.Add(1)
		return
	}

	ms := float64(duration.Microseconds()) / 1000.0
	m.totalSuccess.Add(1)
	m.windowCount.Add(1)

	m.mu.Lock()
	m.windowLatency = append(m.windowLatency, ms)
	m.totalLatency = append(m.totalLatency, ms)
	m.mu.Unlock()
}

func (m *metrics) snapshotWindow() (count, errs uint64, latencies []float64) {
	count = m.windowCount.Swap(0)
	errs = m.windowErrors.Swap(0)
	m.mu.Lock()
	latencies = append([]float64(nil), m.windowLatency...)
	m.windowLatency = m.windowLatency[:0]
	m.mu.Unlock()
	return
}

func (m *metrics) snapshotTotal() (success, failures uint64, latencies []float64) {
	success = m.totalSuccess.Load()
	failures = m.totalFailure.Load()
	m.mu.Lock()
	latencies = append([]float64(nil), m.totalLatency...)
	m.mu.Unlock()
	return
}

type latencyStats struct{ P95, P99, Max float64 }

func calculateLatencyStats(latencies []float64) latencyStats {
	if len(latencies) == 0 {
		return latencyStats{}
	}
	sorted := append([]float64(nil), latencies...)
	sort.Float64s(sorted)
	return latencyStats{P95: quantile(sorted, 0.95), P99: quantile(sorted, 0.99), Max: sorted[len(sorted)-1]}
}

func quantile(sorted []float64, q float64) float64 {
	if len(sorted) == 0 {
		return 0
	}
	if q <= 0 {
		return sorted[0]
	}
	if q >= 1 {
		return sorted[len(sorted)-1]
	}
	idx := int(math.Ceil(q*float64(len(sorted)))) - 1
	if idx < 0 {
		idx = 0
	}
	if idx >= len(sorted) {
		idx = len(sorted) - 1
	}
	return sorted[idx]
}

func parseFlags() (config, error) {
	cfg := config{}
	flag.StringVar(&cfg.dsn, "dsn", "root:root@tcp(127.0.0.1:3306)/mysql", "MySQL DSN (user:pass@tcp(host:port)/db)")
	flag.StringVar(&cfg.query, "query", "SELECT 1", "SQL query")
	flag.IntVar(&cfg.concurrency, "concurrency", 16, "Number of concurrent workers")
	flag.DurationVar(&cfg.duration, "duration", 30*time.Second, "Benchmark duration")
	flag.DurationVar(&cfg.reportInterval, "report-interval", 5*time.Second, "Statistics report interval")
	flag.StringVar(&cfg.mysqlBin, "mysql-bin", "mysql", "Path to mysql client binary")
	flag.Parse()

	if cfg.concurrency <= 0 || cfg.duration <= 0 || cfg.reportInterval <= 0 {
		return cfg, errors.New("concurrency, duration, and report-interval must be > 0")
	}
	if cfg.dsn == "" || cfg.query == "" {
		return cfg, errors.New("dsn and query must not be empty")
	}
	return cfg, nil
}

func runQuery(ctx context.Context, bin string, t mysqlTarget, query string) error {
	cmd := exec.CommandContext(ctx, bin,
		"-h", t.host,
		"-P", t.port,
		"-u", t.user,
		fmt.Sprintf("-p%s", t.pass),
		"-D", t.db,
		"-Nse", query,
	)
	return cmd.Run()
}

func worker(ctx context.Context, cfg config, target mysqlTarget, m *metrics) {
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		start := time.Now()
		err := runQuery(ctx, cfg.mysqlBin, target, cfg.query)
		m.record(time.Since(start), err)
	}
}

func reporter(ctx context.Context, m *metrics, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			wc, we, wl := m.snapshotWindow()
			ws := calculateLatencyStats(wl)
			ts, te, tl := m.snapshotTotal()
			tot := calculateLatencyStats(tl)
			elapsed := time.Since(m.startedAt).Seconds()
			fmt.Printf("[%s] interval_ok=%d interval_err=%d interval_p95=%.2fms interval_p99=%.2fms interval_max=%.2fms total_ok=%d total_err=%d total_tps=%.2f total_p95=%.2fms total_p99=%.2fms total_max=%.2fms\n",
				time.Now().Format(time.RFC3339), wc, we, ws.P95, ws.P99, ws.Max, ts, te, float64(ts)/elapsed, tot.P95, tot.P99, tot.Max)
		}
	}
}

func main() {
	cfg, err := parseFlags()
	if err != nil {
		fmt.Fprintln(os.Stderr, "invalid args:", err)
		os.Exit(2)
	}
	target, err := parseDSN(cfg.dsn)
	if err != nil {
		fmt.Fprintln(os.Stderr, "invalid dsn:", err)
		os.Exit(2)
	}
	if _, err := exec.LookPath(cfg.mysqlBin); err != nil {
		fmt.Fprintf(os.Stderr, "mysql binary %q not found: %v\n", cfg.mysqlBin, err)
		os.Exit(1)
	}

	ctx, cancel := context.WithTimeout(context.Background(), cfg.duration)
	defer cancel()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	defer signal.Stop(sigCh)
	go func() {
		select {
		case <-sigCh:
			cancel()
		case <-ctx.Done():
		}
	}()

	if err := runQuery(ctx, cfg.mysqlBin, target, cfg.query); err != nil {
		fmt.Fprintln(os.Stderr, "warmup query failed:", err)
		os.Exit(1)
	}

	m := &metrics{startedAt: time.Now()}
	fmt.Printf("Starting mysqlbench: dsn=%q query=%q concurrency=%d duration=%s report_interval=%s\n", cfg.dsn, cfg.query, cfg.concurrency, cfg.duration, cfg.reportInterval)

	var wg sync.WaitGroup
	wg.Add(cfg.concurrency)
	for i := 0; i < cfg.concurrency; i++ {
		go func() { defer wg.Done(); worker(ctx, cfg, target, m) }()
	}
	go reporter(ctx, m, cfg.reportInterval)

	<-ctx.Done()
	wg.Wait()
	ts, te, tl := m.snapshotTotal()
	st := calculateLatencyStats(tl)
	elapsed := time.Since(m.startedAt).Seconds()
	fmt.Printf("Final summary: ok=%d err=%d elapsed=%.2fs tps=%.2f p95=%.2fms p99=%.2fms max=%.2fms\n", ts, te, elapsed, float64(ts)/elapsed, st.P95, st.P99, st.Max)
}
