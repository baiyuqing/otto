package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"
)

func main() {
	cfg, err := parseFlags()
	if err != nil {
		fmt.Fprintln(os.Stderr, "invalid args:", err)
		os.Exit(2)
	}

	run, cleanup, err := makeQueryRunner(cfg)
	if err != nil {
		fmt.Fprintln(os.Stderr, "failed to create query runner:", err)
		os.Exit(1)
	}
	defer cleanup()

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

	dsnLog, err := formatDSNForLog(cfg.dsn)
	if err != nil {
		fmt.Fprintf(os.Stderr, "warning: failed to parse dsn for logging: %v\n", err)
		dsnLog = "unparsed"
	}
	fmt.Printf("Starting mysqlbench: dsn={%s} query=%q connection_mode=%s concurrency=%d duration=%s report_interval=%s\n", dsnLog, cfg.query, cfg.connectionMode, cfg.concurrency, cfg.duration, cfg.reportInterval)

	if err := run(ctx); err != nil {
		fmt.Fprintln(os.Stderr, "warmup query failed:", err)
		os.Exit(1)
	}

	m := newMetrics(time.Now())
	startPrometheusServer(ctx, cfg, m)

	var wg sync.WaitGroup
	wg.Add(cfg.concurrency)
	for i := 0; i < cfg.concurrency; i++ {
		go func() { defer wg.Done(); worker(ctx, run, m) }()
	}
	go reporter(ctx, m, cfg.reportInterval)

	<-ctx.Done()
	wg.Wait()
	ts, te, tl := m.snapshotTotal()
	elapsed := time.Since(m.startedAt).Seconds()
	fmt.Printf("Final summary: ok=%d err=%d elapsed=%.2fs tps=%.2f p95=%.2fms p99=%.2fms max=%.2fms\n", ts, te, elapsed, float64(ts)/elapsed, tl.quantile(0.95), tl.quantile(0.99), tl.maxMS)
}
