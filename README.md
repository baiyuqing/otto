# mysqlbench

[中文文档 (简体)](./README.zh-CN.md)

`mysqlbench` is a lightweight MySQL workload probe written in Go.

It repeatedly executes a SQL statement (default: `SELECT 1`) using Go's MySQL client (`database/sql` + `go-sql-driver/mysql`) and reports live latency/throughput statistics so you can quickly assess basic database responsiveness under concurrent load.

## What it does

- Parses MySQL DSNs in the form `user:pass@tcp(host:port)/db`.
- Spawns configurable concurrent workers.
- Runs for a fixed duration (or until interrupted with `SIGINT`/`SIGTERM`).
- Prints periodic interval and cumulative metrics:
  - Success and error counts
  - TPS (transactions per second)
  - Latency `p95`, `p99`, and `max`
- Prints a final benchmark summary at the end.

## Requirements

- Go 1.24+
- A reachable MySQL-compatible endpoint

## Build

```bash
go build -o mysqlbench .
```

## Run

```bash
./mysqlbench \
  -dsn 'root:password@tcp(127.0.0.1:3306)/mysql' \
  -concurrency 32 \
  -duration 1m \
  -report-interval 5s \
  -query 'SELECT 1'
```

## Flags

- `-dsn` (default: `root:root@tcp(127.0.0.1:3306)/mysql`)
  - MySQL DSN in `user:pass@tcp(host:port)/db` format.
- `-query` (default: `SELECT 1`)
  - SQL executed for each operation.
- `-concurrency` (default: `16`)
  - Number of concurrent workers.
- `-duration` (default: `30s`)
  - Total benchmark runtime.
- `-report-interval` (default: `5s`)
  - Periodic reporting cadence.

## Output format

During execution, `mysqlbench` emits lines like:

```text
[2026-01-01T12:00:00Z] interval_ok=160 interval_err=0 interval_p95=3.12ms interval_p99=4.80ms interval_max=9.42ms total_ok=320 total_err=0 total_tps=64.00 total_p95=3.30ms total_p99=5.01ms total_max=9.42ms
```

At completion:

```text
Final summary: ok=12345 err=12 elapsed=60.01s tps=205.72 p95=4.10ms p99=8.33ms max=42.71ms
```

## Notes and caveats

- Query execution uses pooled Go MySQL connections; there is no per-operation process-spawn overhead.
- Use a least-privilege database account for benchmarking to reduce risk when running write queries.

## Development

Run tests:

```bash
go test ./...
```

## Repository layout

- `main.go` — benchmark CLI implementation.
- `main_test.go` — unit tests for latency quantile behavior.
- `skills/network-diagnostics/` — an additional reusable diagnostics skill (independent from `mysqlbench`) with:
  - `SKILL.md` instructions,
  - `scripts/network_triage.sh` helper script,
  - `references/metrics-cheatsheet.md` interpretation guide.
