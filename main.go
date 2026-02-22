package main

import (
	"context"
	"database/sql"
	"errors"
	"flag"
	"fmt"
	"math"
	"net/http"
	"os"
	"os/signal"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	_ "github.com/go-sql-driver/mysql"
)

type config struct {
	dsn              string
	query            string
	concurrency      int
	duration         time.Duration
	reportInterval   time.Duration
	prometheusListen string
	prometheusPath   string
}

type histogram struct {
	bounds []float64
	counts []atomic.Uint64
	sumMS  atomic.Uint64
	count  atomic.Uint64
	maxMS  atomic.Uint64
}

func newHistogram(bounds []float64) *histogram {
	h := &histogram{bounds: append([]float64(nil), bounds...), counts: make([]atomic.Uint64, len(bounds)+1)}
	return h
}

func (h *histogram) observe(ms float64) {
	h.count.Add(1)
	h.sumMS.Add(uint64(ms * 1000))
	for {
		old := h.maxMS.Load()
		newMax := uint64(ms * 1000)
		if newMax <= old || h.maxMS.CompareAndSwap(old, newMax) {
			break
		}
	}
	idx := sort.SearchFloat64s(h.bounds, ms)
	h.counts[idx].Add(1)
}

func (h *histogram) snapshot() histogramSnapshot {
	hs := histogramSnapshot{bucketCounts: make([]uint64, len(h.counts)), bounds: append([]float64(nil), h.bounds...)}
	hs.count = h.count.Load()
	hs.sumMS = float64(h.sumMS.Load()) / 1000
	hs.maxMS = float64(h.maxMS.Load()) / 1000
	for i := range h.counts {
		hs.bucketCounts[i] = h.counts[i].Load()
	}
	return hs
}

func (h *histogram) snapshotAndReset() histogramSnapshot {
	hs := histogramSnapshot{bucketCounts: make([]uint64, len(h.counts)), bounds: append([]float64(nil), h.bounds...)}
	hs.count = h.count.Swap(0)
	hs.sumMS = float64(h.sumMS.Swap(0)) / 1000
	hs.maxMS = float64(h.maxMS.Swap(0)) / 1000
	for i := range h.counts {
		hs.bucketCounts[i] = h.counts[i].Swap(0)
	}
	return hs
}

type histogramSnapshot struct {
	bounds       []float64
	bucketCounts []uint64
	sumMS        float64
	count        uint64
	maxMS        float64
}

func (h histogramSnapshot) quantile(q float64) float64 {
	if h.count == 0 {
		return 0
	}
	target := uint64(math.Ceil(float64(h.count) * q))
	if target == 0 {
		target = 1
	}
	var cumulative uint64
	for i, c := range h.bucketCounts {
		cumulative += c
		if cumulative >= target {
			if i < len(h.bounds) {
				return h.bounds[i]
			}
			return h.maxMS
		}
	}
	return h.maxMS
}

type metrics struct {
	totalSuccess atomic.Uint64
	totalFailure atomic.Uint64
	windowErrors atomic.Uint64

	totalHistogram  *histogram
	windowHistogram *histogram
	startedAt       time.Time
}

func newMetrics(start time.Time) *metrics {
	// Upper bounds in milliseconds. Keep this list compact to minimize memory/CPU overhead.
	bounds := []float64{0.25, 0.5, 1, 2, 5, 10, 20, 50, 100, 250, 500, 1000, 2000, 5000, 10000}
	return &metrics{
		totalHistogram:  newHistogram(bounds),
		windowHistogram: newHistogram(bounds),
		startedAt:       start,
	}
}

func (m *metrics) record(duration time.Duration, err error) {
	if err != nil {
		m.totalFailure.Add(1)
		m.windowErrors.Add(1)
		return
	}

	ms := float64(duration.Microseconds()) / 1000.0
	m.totalSuccess.Add(1)
	m.totalHistogram.observe(ms)
	m.windowHistogram.observe(ms)
}

func (m *metrics) snapshotWindow() (errs uint64, hs histogramSnapshot) {
	errs = m.windowErrors.Swap(0)
	hs = m.windowHistogram.snapshotAndReset()
	return
}

func (m *metrics) snapshotTotal() (success, failures uint64, hs histogramSnapshot) {
	success = m.totalSuccess.Load()
	failures = m.totalFailure.Load()
	hs = m.totalHistogram.snapshot()
	return
}

func parseFlags() (config, error) {
	cfg := config{}
	flag.StringVar(&cfg.dsn, "dsn", "root:root@tcp(127.0.0.1:3306)/mysql", "MySQL DSN")
	flag.StringVar(&cfg.query, "query", "SELECT 1", "SQL query")
	flag.IntVar(&cfg.concurrency, "concurrency", 16, "Number of concurrent workers")
	flag.DurationVar(&cfg.duration, "duration", 30*time.Second, "Benchmark duration")
	flag.DurationVar(&cfg.reportInterval, "report-interval", 5*time.Second, "Statistics report interval")
	flag.StringVar(&cfg.prometheusListen, "prometheus-listen", "", "Prometheus metrics listen address (e.g. :9090). Empty disables endpoint")
	flag.StringVar(&cfg.prometheusPath, "prometheus-path", "/metrics", "Prometheus metrics path")
	flag.Parse()

	if cfg.concurrency <= 0 || cfg.duration <= 0 || cfg.reportInterval <= 0 {
		return cfg, errors.New("concurrency, duration, and report-interval must be > 0")
	}
	if cfg.dsn == "" || cfg.query == "" {
		return cfg, errors.New("dsn and query must not be empty")
	}
	if cfg.prometheusListen != "" && !strings.HasPrefix(cfg.prometheusPath, "/") {
		return cfg, errors.New("prometheus-path must start with '/'")
	}
	return cfg, nil
}

func runQuery(ctx context.Context, db *sql.DB, query string) error {
	tx, err := db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}

	rows, err := tx.QueryContext(ctx, query)
	if err != nil {
		if strings.Contains(strings.ToLower(err.Error()), "does not return rows") {
			_, execErr := tx.ExecContext(ctx, query)
			if execErr != nil {
				_ = tx.Rollback()
				return execErr
			}
			return tx.Commit()
		}
		_ = tx.Rollback()
		return err
	}
	defer rows.Close()

	cols, err := rows.Columns()
	if err != nil {
		_ = tx.Rollback()
		return err
	}
	vals := make([]any, len(cols))
	scanArgs := make([]any, len(cols))
	for i := range vals {
		scanArgs[i] = &vals[i]
	}
	for rows.Next() {
		if err := rows.Scan(scanArgs...); err != nil {
			_ = tx.Rollback()
			return err
		}
	}
	if err := rows.Err(); err != nil {
		_ = tx.Rollback()
		return err
	}
	return tx.Commit()
}

func worker(ctx context.Context, cfg config, db *sql.DB, m *metrics) {
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		start := time.Now()
		err := runQuery(ctx, db, cfg.query)
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
			we, wl := m.snapshotWindow()
			ts, te, tl := m.snapshotTotal()
			elapsed := time.Since(m.startedAt).Seconds()
			wm := readMemStats()
			fmt.Printf("[%s] interval_ok=%d interval_err=%d interval_p95=%.2fms interval_p99=%.2fms interval_max=%.2fms total_ok=%d total_err=%d total_tps=%.2f total_p95=%.2fms total_p99=%.2fms total_max=%.2fms mem_alloc_mb=%.2f mem_sys_mb=%.2f\n",
				time.Now().Format(time.RFC3339), wl.count, we, wl.quantile(0.95), wl.quantile(0.99), wl.maxMS,
				ts, te, float64(ts)/elapsed, tl.quantile(0.95), tl.quantile(0.99), tl.maxMS,
				float64(wm.Alloc)/(1024*1024), float64(wm.Sys)/(1024*1024))
		}
	}
}

func startPrometheusServer(ctx context.Context, cfg config, m *metrics) {
	if cfg.prometheusListen == "" {
		return
	}
	mux := http.NewServeMux()
	mux.HandleFunc(cfg.prometheusPath, func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
		writePrometheusMetrics(w, m)
	})

	srv := &http.Server{Addr: cfg.prometheusListen, Handler: mux}
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
	}()
	go func() {
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			fmt.Fprintf(os.Stderr, "prometheus server error: %v\n", err)
		}
	}()
	fmt.Printf("Prometheus metrics endpoint listening at http://%s%s\n", cfg.prometheusListen, cfg.prometheusPath)
}

func writePrometheusMetrics(w http.ResponseWriter, m *metrics) {
	ts, te, hs := m.snapshotTotal()
	elapsed := time.Since(m.startedAt).Seconds()
	wm := readMemStats()

	fmt.Fprintln(w, "# HELP mysqlbench_success_total Total successful queries.")
	fmt.Fprintln(w, "# TYPE mysqlbench_success_total counter")
	fmt.Fprintf(w, "mysqlbench_success_total %d\n", ts)

	fmt.Fprintln(w, "# HELP mysqlbench_failure_total Total failed queries.")
	fmt.Fprintln(w, "# TYPE mysqlbench_failure_total counter")
	fmt.Fprintf(w, "mysqlbench_failure_total %d\n", te)

	fmt.Fprintln(w, "# HELP mysqlbench_tps Average TPS since benchmark start.")
	fmt.Fprintln(w, "# TYPE mysqlbench_tps gauge")
	fmt.Fprintf(w, "mysqlbench_tps %.6f\n", float64(ts)/elapsed)

	fmt.Fprintln(w, "# HELP mysqlbench_latency_ms Latency histogram in milliseconds.")
	fmt.Fprintln(w, "# TYPE mysqlbench_latency_ms histogram")
	var cumulative uint64
	for i, c := range hs.bucketCounts {
		cumulative += c
		if i < len(hs.bounds) {
			fmt.Fprintf(w, "mysqlbench_latency_ms_bucket{le=\"%s\"} %d\n", trimFloat(hs.bounds[i]), cumulative)
			continue
		}
		fmt.Fprintf(w, "mysqlbench_latency_ms_bucket{le=\"+Inf\"} %d\n", cumulative)
	}
	fmt.Fprintf(w, "mysqlbench_latency_ms_sum %.3f\n", hs.sumMS)
	fmt.Fprintf(w, "mysqlbench_latency_ms_count %d\n", hs.count)

	fmt.Fprintln(w, "# HELP mysqlbench_memory_alloc_bytes Current heap allocation in bytes.")
	fmt.Fprintln(w, "# TYPE mysqlbench_memory_alloc_bytes gauge")
	fmt.Fprintf(w, "mysqlbench_memory_alloc_bytes %d\n", wm.Alloc)

	fmt.Fprintln(w, "# HELP mysqlbench_memory_sys_bytes Total bytes obtained from system.")
	fmt.Fprintln(w, "# TYPE mysqlbench_memory_sys_bytes gauge")
	fmt.Fprintf(w, "mysqlbench_memory_sys_bytes %d\n", wm.Sys)
}

func trimFloat(v float64) string {
	return strconv.FormatFloat(v, 'f', -1, 64)
}

func readMemStats() runtime.MemStats {
	var ms runtime.MemStats
	runtime.ReadMemStats(&ms)
	return ms
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

func main() {
	cfg, err := parseFlags()
	if err != nil {
		fmt.Fprintln(os.Stderr, "invalid args:", err)
		os.Exit(2)
	}

	db, err := sql.Open("mysql", cfg.dsn)
	if err != nil {
		fmt.Fprintln(os.Stderr, "failed to open dsn:", err)
		os.Exit(1)
	}
	defer db.Close()
	db.SetMaxOpenConns(cfg.concurrency)
	db.SetMaxIdleConns(cfg.concurrency)

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

	if err := runQuery(ctx, db, cfg.query); err != nil {
		fmt.Fprintln(os.Stderr, "warmup query failed:", err)
		os.Exit(1)
	}

	m := newMetrics(time.Now())
	fmt.Printf("Starting mysqlbench: dsn=%q query=%q concurrency=%d duration=%s report_interval=%s\n", cfg.dsn, cfg.query, cfg.concurrency, cfg.duration, cfg.reportInterval)
	startPrometheusServer(ctx, cfg, m)

	var wg sync.WaitGroup
	wg.Add(cfg.concurrency)
	for i := 0; i < cfg.concurrency; i++ {
		go func() { defer wg.Done(); worker(ctx, cfg, db, m) }()
	}
	go reporter(ctx, m, cfg.reportInterval)

	<-ctx.Done()
	wg.Wait()
	ts, te, tl := m.snapshotTotal()
	elapsed := time.Since(m.startedAt).Seconds()
	fmt.Printf("Final summary: ok=%d err=%d elapsed=%.2fs tps=%.2f p95=%.2fms p99=%.2fms max=%.2fms\n", ts, te, elapsed, float64(ts)/elapsed, tl.quantile(0.95), tl.quantile(0.99), tl.maxMS)
}
