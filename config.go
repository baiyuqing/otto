package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"strings"
	"time"
)

type config struct {
	dsn              string
	query            string
	connectionMode   string
	concurrency      int
	duration         time.Duration
	reportInterval   time.Duration
	prometheusListen string
	prometheusPath   string
}

const (
	connectionModeLongRunning = "long-running"
	connectionModePerTxn      = "per-transaction"
)

func parseFlags() (config, error) {
	cfg := config{}
	flag.StringVar(&cfg.dsn, "dsn", "mock_user:mock_password@tcp(127.0.0.1:3306)/mysql", "MySQL DSN")
	flag.StringVar(&cfg.query, "query", "SELECT 1", "SQL query")
	flag.StringVar(&cfg.connectionMode, "connection-mode", connectionModeLongRunning, "Connection mode: long-running or per-transaction")
	flag.IntVar(&cfg.concurrency, "concurrency", 16, "Number of concurrent workers")
	flag.DurationVar(&cfg.duration, "duration", 30*time.Second, "Benchmark duration")
	flag.DurationVar(&cfg.reportInterval, "report-interval", 5*time.Second, "Statistics report interval")
	flag.StringVar(&cfg.prometheusListen, "prometheus-listen", "", "Prometheus metrics listen address (e.g. :9090). Empty disables endpoint")
	flag.StringVar(&cfg.prometheusPath, "prometheus-path", "/metrics", "Prometheus metrics path")
	if err := flag.CommandLine.Parse(normalizeFlagArgs(os.Args[1:])); err != nil {
		return cfg, err
	}
	cfg.dsn = trimMatchingQuotes(strings.TrimSpace(cfg.dsn))

	if cfg.concurrency <= 0 || cfg.duration <= 0 || cfg.reportInterval <= 0 {
		return cfg, errors.New("concurrency, duration, and report-interval must be > 0")
	}
	if cfg.dsn == "" || cfg.query == "" {
		return cfg, errors.New("dsn and query must not be empty")
	}
	if !isValidConnectionMode(cfg.connectionMode) {
		return cfg, fmt.Errorf("invalid connection-mode %q: must be %q or %q", cfg.connectionMode, connectionModeLongRunning, connectionModePerTxn)
	}
	if cfg.prometheusListen != "" && !strings.HasPrefix(cfg.prometheusPath, "/") {
		return cfg, errors.New("prometheus-path must start with '/'")
	}
	return cfg, nil
}

func isValidConnectionMode(mode string) bool {
	return mode == connectionModeLongRunning || mode == connectionModePerTxn
}

func normalizeFlagArgs(args []string) []string {
	normalized := make([]string, 0, len(args))
	for _, arg := range args {
		trimmed := strings.TrimSpace(arg)
		if strings.HasPrefix(trimmed, "-dsn ") {
			parts := strings.SplitN(trimmed, " ", 2)
			if len(parts) == 2 {
				normalized = append(normalized, "-dsn", trimMatchingQuotes(strings.TrimSpace(parts[1])))
				continue
			}
		}
		normalized = append(normalized, arg)
	}
	return normalized
}

func trimMatchingQuotes(s string) string {
	if len(s) < 2 {
		return s
	}
	if s[0] == '\'' && s[len(s)-1] == '\'' {
		return s[1 : len(s)-1]
	}
	if s[0] == '"' && s[len(s)-1] == '"' {
		return s[1 : len(s)-1]
	}
	return s
}
