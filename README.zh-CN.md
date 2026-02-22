# mysqlbench

`mysqlbench` 是一个用 Go 编写的轻量级 MySQL 压测探针工具。

它会持续执行 SQL 语句（默认：`SELECT 1`），通过 Go MySQL 客户端（`database/sql` + `go-sql-driver/mysql`）发起请求，并实时输出延迟与吞吐统计，帮助你快速评估数据库在并发负载下的基础响应能力。

## 功能概览

- 解析 `user:pass@tcp(host:port)/db` 格式的 MySQL DSN。
- 按配置启动并发 worker。
- 在固定时长内运行（或收到 `SIGINT`/`SIGTERM` 时提前结束）。
- 周期性输出区间与累计指标：
  - 成功/失败次数
  - TPS（每秒事务数）
  - 延迟 `p95`、`p99`、`max`
- 结束时输出最终汇总结果。

## 运行要求

- Go 1.24+
- 可访问的 MySQL（或兼容）实例

## 构建

```bash
go build -o mysqlbench .
```

## 运行示例

```bash
./mysqlbench \
  -dsn 'root:password@tcp(127.0.0.1:3306)/mysql' \
  -concurrency 32 \
  -duration 1m \
  -report-interval 5s \
  -query 'SELECT 1'
```

## 参数说明

- `-dsn`（默认：`root:root@tcp(127.0.0.1:3306)/mysql`）
  - MySQL DSN，格式为 `user:pass@tcp(host:port)/db`。
- `-query`（默认：`SELECT 1`）
  - 每次操作执行的 SQL 语句。
- `-concurrency`（默认：`16`）
  - 并发 worker 数量。
- `-duration`（默认：`30s`）
  - 压测总时长。
- `-report-interval`（默认：`5s`）
  - 周期性统计输出间隔。

## 输出格式

运行中会输出类似如下的统计行：

```text
[2026-01-01T12:00:00Z] interval_ok=160 interval_err=0 interval_p95=3.12ms interval_p99=4.80ms interval_max=9.42ms total_ok=320 total_err=0 total_tps=64.00 total_p95=3.30ms total_p99=5.01ms total_max=9.42ms
```

结束时会输出：

```text
Final summary: ok=12345 err=12 elapsed=60.01s tps=205.72 p95=4.10ms p99=8.33ms max=42.71ms
```

## 注意事项

- 查询通过 Go 连接池复用连接执行；统计结果不再包含每次进程拉起开销。
- 建议使用最小权限数据库账号执行压测，尤其是执行写入语句时。

## 开发与测试

运行测试：

```bash
go test ./...
```

## 仓库结构

- `main.go`：压测 CLI 主实现。
- `main_test.go`：延迟分位数逻辑的单元测试。
- `skills/network-diagnostics/`：独立于 `mysqlbench` 的网络诊断技能目录，包含：
  - `SKILL.md` 使用说明
  - `scripts/network_triage.sh` 诊断脚本
  - `references/metrics-cheatsheet.md` 指标速查说明
