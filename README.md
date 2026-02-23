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
  - Runtime memory usage (`mem_alloc_mb`, `mem_sys_mb`)
- Optionally exposes Prometheus-compatible metrics over HTTP.
- Prints a final benchmark summary at the end.

## Requirements

- Go 1.25+
- A reachable MySQL-compatible endpoint

## TLS support

`mysqlbench` uses `go-sql-driver/mysql`, so TLS is configured through the DSN parameters.

Common options:

- `tls=true` — require TLS (certificate is verified against host trust store).
- `tls=skip-verify` — enable TLS without certificate/hostname verification (testing only).
- `tls=preferred` — use TLS when available, otherwise allow non-TLS fallback.

Example:

```bash
./mysqlbench \
  -dsn 'root:password@tcp(127.0.0.1:3306)/mysql?tls=true' \
  -concurrency 32 \
  -duration 1m
```

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
- `-connection-mode` (default: `long-running`)
  - Connection strategy. `long-running` uses a shared pool for the full run; `per-transaction` opens and closes a new DB connection for each transaction.
- `-concurrency` (default: `16`)
  - Number of concurrent workers.
- `-duration` (default: `30s`)
  - Total benchmark runtime.
- `-report-interval` (default: `5s`)
  - Periodic reporting cadence.
- `-prometheus-listen` (default: empty)
  - HTTP listen address for Prometheus scraping, e.g. `:9090`. Empty disables endpoint.
- `-prometheus-path` (default: `/metrics`)
  - HTTP path for Prometheus metrics.

## Prometheus metrics

Enable the metrics endpoint with `-prometheus-listen`:

```bash
./mysqlbench \
  -dsn 'root:password@tcp(127.0.0.1:3306)/mysql' \
  -prometheus-listen ':9090' \
  -prometheus-path '/metrics'
```

Then scrape `http://localhost:9090/metrics`. Exported series include success/error counters, TPS gauge, latency histogram, and Go memory gauges.

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

- By default (`-connection-mode long-running`), queries use pooled Go MySQL connections.
- Use `-connection-mode per-transaction` to force each transaction to open a fresh connection and close it afterwards.
- Use a least-privilege database account for benchmarking to reduce risk when running write queries.

## Docker

Build image:

```bash
docker build -t mysqlbench:latest .
```

Build a multi-arch image (x86_64 + arm64) with Buildx:

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t mysqlbench:latest \
  .
```

Run benchmark from container:

```bash
docker run --rm mysqlbench:latest \
  -dsn 'root:password@tcp(host.docker.internal:3306)/mysql' \
  -concurrency 32 \
  -duration 1m
```

Open a shell with built-in diagnostics tools (`bash`, `curl`, `ping`, `nc`, `dig`, `mysql`):

```bash
docker run --rm -it --entrypoint bash mysqlbench:latest
```

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
