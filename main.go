package main

import (
	"context"
	"database/sql"
	"errors"
	"flag"
	"fmt"
	"math"
	"os"
	"os/signal"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	_ "github.com/go-sql-driver/mysql"
)

type config struct {
	dsn            string
	query          string
	concurrency    int
	duration       time.Duration
	reportInterval time.Duration
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
	flag.StringVar(&cfg.dsn, "dsn", "root:root@tcp(127.0.0.1:3306)/mysql", "MySQL DSN")
	flag.StringVar(&cfg.query, "query", "SELECT 1", "SQL query")
	flag.IntVar(&cfg.concurrency, "concurrency", 16, "Number of concurrent workers")
	flag.DurationVar(&cfg.duration, "duration", 30*time.Second, "Benchmark duration")
	flag.DurationVar(&cfg.reportInterval, "report-interval", 5*time.Second, "Statistics report interval")
	flag.Parse()

	if cfg.concurrency <= 0 || cfg.duration <= 0 || cfg.reportInterval <= 0 {
		return cfg, errors.New("concurrency, duration, and report-interval must be > 0")
	}
	if cfg.dsn == "" || cfg.query == "" {
		return cfg, errors.New("dsn and query must not be empty")
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

	m := &metrics{startedAt: time.Now()}
	fmt.Printf("Starting mysqlbench: dsn=%q query=%q concurrency=%d duration=%s report_interval=%s\n", cfg.dsn, cfg.query, cfg.concurrency, cfg.duration, cfg.reportInterval)

	var wg sync.WaitGroup
	wg.Add(cfg.concurrency)
	for i := 0; i < cfg.concurrency; i++ {
		go func() { defer wg.Done(); worker(ctx, cfg, db, m) }()
	}
	go reporter(ctx, m, cfg.reportInterval)

	<-ctx.Done()
	wg.Wait()
	ts, te, tl := m.snapshotTotal()
	st := calculateLatencyStats(tl)
	elapsed := time.Since(m.startedAt).Seconds()
	fmt.Printf("Final summary: ok=%d err=%d elapsed=%.2fs tps=%.2f p95=%.2fms p99=%.2fms max=%.2fms\n", ts, te, elapsed, float64(ts)/elapsed, st.P95, st.P99, st.Max)
}
