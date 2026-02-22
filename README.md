# mysqlbench

A lightweight MySQL benchmark tool written in Go for transaction-style checks (default query: `SELECT 1`).

It exposes benchmark parameters including MySQL DSN, concurrency, and time duration, and prints periodic statistics with latency **P95**, **P99**, and **Max**.

## Build

```bash
go build -o mysqlbench .
```

## Usage

```bash
./mysqlbench \
  -dsn 'root:password@tcp(127.0.0.1:3306)/mysql' \
  -concurrency 32 \
  -duration 1m \
  -report-interval 5s \
  -query 'SELECT 1'
```

## Parameters

- `-dsn`: MySQL DSN in `user:pass@tcp(host:port)/db` format
- `-query`: SQL query executed per transaction
- `-concurrency`: number of concurrent workers
- `-duration`: total benchmark run duration
- `-report-interval`: interval for periodic statistics output
- `-mysql-bin`: path to `mysql` CLI binary (default: `mysql`)

## Notes

- This implementation uses the local `mysql` CLI client process for query execution.
- Ensure `mysql` is installed and available in `PATH`, or set `-mysql-bin`.
