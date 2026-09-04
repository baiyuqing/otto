# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

@AGENTS.md

AGENTS.md is the canonical rulebook: Stage 1 scope (openai-compatible provider only), package boundaries, mandatory TDD workflow, offline-test requirements, and secrets rules. Follow it exactly. This file only adds what AGENTS.md does not cover.

## Commands

```bash
make build      # go build -trimpath -o ./otto ./cmd/otto
make check      # full CI gate: fmt, vet, staticcheck, test, test-race, git diff --check
make test       # go test ./... (offline; no network, credentials, or TTY needed)
make test-tui   # PTY smoke test: go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=1
```

Single test: `go test ./internal/tui -run TestName -count=1` (tests live next to their package).

## Architecture

Otto is a macOS coding-agent CLI. The wiring, from process start to a rendered response:

1. **`cmd/otto/main.go` + `runtime_builder.go`** — parse flags, resolve the runtime (provider, model, API key, session) from flags > `OTTO_*` env vars > TOML profile > resumed-session defaults (exact precedence is documented in README "Configuration and precedence"), then construct: provider → tool registry → session store → `agent.Agent` → `app.Controller` → frontend.

2. **`internal/app.Controller`** — the concurrency hub between frontends and the agent. Serializes prompts (one active turn; `ErrPromptActive` otherwise), owns session replacement for `/new` and `/resume` (swaps session + runner + runtime info atomically), and exposes `Info()`/`History()` snapshots. Frontends never touch the agent, provider, or session directly — everything goes through the Controller via the factory types (`SessionFactory`, `RunnerFactory`, `ResumeFactory`) injected at startup.

3. **`internal/agent.Agent.Run`** — the turn loop: redact and append the user message, stream a provider completion, execute any tool calls through the registry, append results, loop until the provider finishes without tool calls. Progress is reported as a flat `agent.Event` stream (`text_delta`, `tool_call_started/finished`, `provider_usage`, …) that both frontends render; the Event stream is the only frontend-facing output contract.

4. **`internal/provider`** — a one-method interface: `Complete(ctx, Request, streamCallback) (Response, error)`. `openaicompat` is the sole Stage 1 implementation and owns all HTTP/JSON/SSE wire structs; nothing provider-specific may leak out of that package. `internal/model` holds the provider-neutral message/block/tool types everything else speaks.

5. **`internal/session`** — append-only JSONL in the **Pi session format v3** (`pi_codec.go`/`pi_types.go`), stored under `~/.otto/sessions/<workspace-key>/`. Files are created lazily on the first user prompt (`prepared.go`); `memory.go` backs `--no-session`. `scripts/pi-session-interop.mjs` is an opt-in compatibility probe against a real Pi install — never invoked by default tests.

6. **`internal/memory` + `internal/memory/sqlite`** — an extensible memory core: neutral contracts (`internal/memory`) and a secure local SQLite/FTS5 store/retriever (`internal/memory/sqlite`), wired end to end via `[memory]` TOML config (`internal/config/memory.go`), `runtime_builder.go`, and the `app.Controller` memory facade. Every turn recalls matching records into a request-local, non-persisted context block (`internal/agent/agent.go`); the model has `memory_search`/`remember`/`forget` tools whose writes always land as pending candidates needing human review; both frontends and the standalone `otto memory status|forget` CLI expose explicit management (`/memory search|forget|review`, `/remember`). Still unwired: automatic extraction (`Binding.Observe` is never called) and durability (`Maintenance` — backup/restore/verify — is a permanent stub).

7. **`internal/skill`** — discover on every runner build (startup, `/new`, `/resume`, `/model`), appending a `## Skills` section to the dynamic system prompt; `internal/tool/skill.go` implements the `skill` tool; `internal/config/skills.go` loads `[skills]` TOML; `cmd/otto/main.go` appends existing skill roots to Seatbelt read paths at process start. Within a session the catalog is fixed for prompt-cache stability. See `docs/specs/2026-09-03-skills-design.md`.

8. **`internal/subagent`** — the `agent` tool starts a child `agent.Agent` (fresh in-memory session, parent tools minus the excluded set, same provider/redactor/sandbox) in a goroutine under a `[agents].max_parallel`-slot semaphore (default 4); task records live in `agent.Tasks` on the parent agent; completion notifications are pushed into `agent.Inbox` and `Agent.Run` drains it into `RoleContext` messages (`task_notification`) before each provider request; the REPL and TUI both start a wake turn (empty prompt) when notifications are pending and no turn is active — the TUI also renders a task panel (queued/running tasks) between the transcript and the input box, and both frontends expose `/tasks` and `/task <id|name>`/`/task cancel <id|name>`; `otto serve` starts the same wake turn from its per-session goroutine (item 10). Named definitions (`AGENT.md`) are discovered from `[agents].paths` on every runner build and rendered as a `## Agents` system-prompt section appended after `## Skills`; `context: inherit` seeds the child's memory from `Config.ParentSession` via `subagent.InheritSnapshot`; `[agents].enabled = false` drops the runner and the three tools. See `docs/specs/2026-09-03-subagents-design.md`.

9. **Frontends** — `internal/tui` (inline Bubble Tea renderer: finished transcript printed to terminal scrollback, live region for in-progress turn and input, Markdown rendering, slash-command completion, `/resume` modal) and `internal/repl` (line-oriented fallback for non-TTY). Selection is `--ui` > `OTTO_UI` > `[ui].mode` > auto (TUI only when stdin **and** stdout are terminals).

10. **`internal/server`** — an HTTP+JSON+SSE frontend over a Unix domain socket, used by `otto serve`: one `app.Controller` per session, a turn event buffer decoupled from the agent's synchronous `emit` callback so a slow client cannot block a turn, hand-rolled Prometheus text exposition (no dependency), and `log/slog` structured request/turn logging. Each open session also runs a goroutine that starts an automatic `trigger: "task"` wake turn whenever a sub-agent task notification is pending and no turn is active, exposes the task registry through `GET/POST /v1/sessions/{id}/tasks…` routes, and reports `otto_tasks_started_total`/`otto_tasks_finished_total{status}`/`otto_tasks_running` metrics. `otto serve` (`cmd/otto/main.go`) reuses the full startup chain and builds each session's `app.Controller` from `runtimeBuilder.buildNewReplacement`/`openReplacement` plus the shared `newController` helper in `cmd/otto/runtime_builder.go`. See `docs/specs/2026-09-03-agent-server-design.md`.

Cross-cutting: `agent.Redactor` scrubs API-key values from text before it reaches the session file; `internal/tool` enforces workspace confinement (canonical paths, symlink resolution) for `read`/`grep`/`find`/`ls`/`write`/`edit`/`skill`, while `bash` is deliberately unsandboxed.
