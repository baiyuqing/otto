# Repository Guidelines

## Scope

- Supported providers: `openai-compatible` (Chat Completions over HTTP with an API key) and `chatgpt` (ChatGPT subscription signed in with `otto login`, Responses API)
- Target platform: macOS

Do not document or implement other providers, and do not list planned providers or roadmap stages in user-facing docs.

## Package boundaries

Keep responsibilities split along the current Go package layout:

- `cmd/otto`: composition root, CLI wiring, flags, process lifecycle, signal handling, and concrete dependency injection
- `internal/agent`: provider/tool orchestration and event emission
- `internal/app`: shared lifecycle, turn admission, session replacement, task/authentication capabilities, profile selection, and session info/history access
- `internal/config`: TOML loading and runtime resolution
- `internal/model`: provider-neutral message/tool types, message/block validation, and shared deep-copy helpers
- `internal/memory`: neutral memory contracts, validation/secret guards, conservative policy, the `Service` implementation composing them with a `Store`/`Retriever`, a null service fallback, and the shared Store conformance harness
- `internal/memory/sqlite`: secure local SQLite/FTS5 store and retriever implementing the memory contracts
- `internal/provider`: neutral provider contract
- `internal/provider/openaicompat`: all OpenAI-compatible Chat Completions HTTP/JSON/SSE wire code
- `internal/provider/openairesponses`: all ChatGPT Responses API HTTP/JSON/SSE wire code
- `internal/auth`: ChatGPT OAuth sign-in (`otto login`/`otto logout`), credential storage at `~/.otto/auth/chatgpt.json`, access-token refresh, and the credential service injected through `app.Authentication`
- `internal/repl`: line-oriented REPL rendering and commands
- `internal/server`: HTTP/JSON/SSE frontend, wire DTOs, per-session turn buffering, and metrics
- `internal/sandbox`: sandbox driver contracts, environment filtering, and conformance helpers
- `internal/session`: in-memory and JSONL session storage
- `internal/skill`: SKILL.md frontmatter parsing, name/description validation, discovery across configured roots, and rendering of the system-prompt listing; `ParseFrontmatter` is exported for `internal/subagent`'s AGENT.md parsing
- `internal/subagent`: child agent construction (`Runner`), task lifecycle, the `agent`/`agent_wait`/`agent_status` tools, shared task-formatting helpers (`format.go`) used by both the REPL and the TUI, AGENT.md definition discovery (`definition.go`, may import `internal/skill` for `ParseFrontmatter`), the `## Agents` prompt section (`prompt.go`), and the `context: inherit` snapshot (`inherit.go`)
- `internal/tool`: workspace validation plus `read`/`grep`/`find`/`ls`/`write`/`edit`/`bash`/`skill`
- `internal/tui`: inline Bubble Tea frontend, transcript rendering, Markdown/tool presentation, key handling, and terminal lifecycle

Rules:

- Keep provider-specific wire structs inside `internal/provider/openaicompat` and `internal/provider/openairesponses`.
- Keep file-tool workspace enforcement inside `internal/tool`.
- Keep session persistence append-only.
- Keep `bash` delegated through `internal/sandbox`; only explicit sandbox `off` may use direct execution, and it still starts in the selected workspace.
- Keep `internal/memory` behind its neutral contracts; the agent loop, tools, and frontends must never reach a Store directly — only through `memory.Binding`/`memory.Reader`/`memory.Proposer` or the `app.Controller` memory facade. Per-turn recall and explicit management (`memory_search`/`remember`/`forget` tools, `/memory`/`/remember` in both frontends, `otto memory status|forget`) are wired end to end via `[memory]` TOML config; model- and human-originated writes always land as pending candidates requiring review. Automatic extraction (`Binding.Observe`) and durability (backup/restore/verify) remain unwired — do not document those as working features.
- Keep `internal/skill` free of imports from other Otto packages; the skill tool's file reads stay confined to the skill directory via `tool.Workspace`. Do not document `/skills`, `/skill`, or `allowed-tools` enforcement as working features.
- Keep `internal/subagent` behind `agent.New`: children are built only through `agent.New`; `internal/agent` knows tasks only through `agent.Tasks`/`agent.Inbox` and never imports `internal/subagent`; frontends reach tasks only through `app.TaskLister`; children never receive `agent*`, `remember`, `forget`, or `memory_search`; child transcripts are not persisted. Definitions cannot add tools outside the child tool set; `tools` only narrows it. `[agents]` is TOML only, same as `[skills]`. Do not document `agent_send`/`agent_cancel`/`agent_report` as working features.

## Core contracts

- Keep dependencies explicit and directed toward shared contracts. Reuse existing helpers and concrete types; add interfaces at real consumer boundaries, not for hypothetical implementations.
- Use `model.Message.Validate`, `model.Block.Validate`, and `model.CloneMessage`/`CloneMessages`/`CloneUsage`. Both Session implementations enforce neutral validation and tool-call/result sequencing on append. Neutral validation permits transient messages without IDs/timestamps; Pi-specific encoding restrictions stay in `session`. When adding reference fields, update deep copies and ownership tests.
- `provider.Response.Message` is the single source of finish reason and usage. `Usage == nil` means unavailable; a non-nil zero usage means explicitly reported zero. Preserve explicit presence through events, task progress, notifications, aggregates, and supported persistence metadata instead of inferring absence from zero counters. Keep legacy Pi normalization in the decoder.
- Keep context associations in typed `ContextMetadata`. Prefer structured TaskID over notification wording; text parsing is only a legacy-history fallback. Preserve append-only Pi v3 compatibility and namespaced optional details, including the explicit-zero usage marker. Do not rewrite old records or invent missing historical metadata.
- `tool.Result.PersistedContent == nil` selects `Content`; a non-nil pointer selects its value, including empty text. Preserve redaction and the current-turn full-result overlay. Reuse tool definitions and assembly helpers, including `tool.BashDefinition`; keep conservative preflight and final registry validation.
- Provider and Tool instances may be shared concurrently. Respect borrowed read-only request/argument data and caller-owned returned data/schema. Per-call provider callbacks are ordered and finish before `Complete` returns; event consumers must copy reference fields before retaining mutable payloads.
- Reuse the shared compaction result payload. Keep HTTP/SSE DTOs separate from internal structs; update `internal/server/openapi.yaml` alongside wire changes and preserve existing field meanings.

## Lifecycle and frontend contracts

- Construct `app.New` with a ready `SessionReplacement`; use `NewSessionBuilder` for new sessions. Do not restore placeholder factories or duplicate runtime construction paths.
- Controller callbacks use nonblocking `RequestClose()`. External lifecycle owners cancel active work as appropriate and call synchronous `Close()` to complete cleanup. An idle close request alone does not release resources. Preserve exactly-once session/runner cleanup, cleanup errors, and post-close Info/History snapshots; do not inspect goroutine IDs or stack text to detect reentrancy.
- Treat `agent.Task` as a query snapshot. Update existing task progress and completion through `MarkRunning`, `RecordProviderStep`, `RecordToolCall`, and `Finish`; preserve task identity, terminal states, and notification-before-Wait ordering. A cancellation request is not proof that execution has stopped.
- Frontends use `app.TaskLister`/`TaskView`, never the mutable registry or raw Inbox. Call `PrepareWake` before publishing a wake turn, then run the claim once or cancel it. Release abandoned claims on cancellation/shutdown. `Updates` is a coalescing single-consumer signal, not a broadcast subscription; add no competing scheduler. Normal and wake turns share cancellation, event delivery, and compaction accounting; one-shot runs propagate wake failures.
- Use `app.Authentication` and `app.SelectProfile` for shared use cases. Credential paths and concrete services belong in the composition root; OAuth and credential files stay in `auth`. Frontends own presentation, not credential persistence. Preserve the startup credential snapshot/restart requirement and refresh backend state after a profile switch even when saving the default fails.

See [the architecture contract design](docs/specs/2026-09-05-architecture-contracts.md) for rationale and compatibility details.

## Working preferences

- Consider cost and efficiency when choosing models: default to the cheapest adequate model for routine work and escalate to a more capable model only when the task requires it.
- When delegating, the parent owns planning, interface decisions, integration review, and acceptance. Give subagents bounded implementation tasks with explicit file ownership; hand off overlapping files before another agent edits them. Preserve other agents' work.

## Development isolation

- Use a dedicated Git worktree and development branch for every feature or bug fix. Never implement directly on `main`.
- When work requires a design or spec, finish the discussion and get explicit approval before writing production code or tests.
- Once the design is approved, carry the agreed work through implementation and verification without repeated confirmation; seek clarification only for material changes to scope or contracts.
- If implementation changes are accidentally made on `main`, restore those changes before continuing in the worktree.

## Go workflow

Primary commands:

```bash
go build -trimpath -o ./otto ./cmd/otto
go test ./...
go test -race ./...
go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=1
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
test -z "$(gofmt -l .)"
git diff --check
```

Use the `go run ...staticcheck@latest` form when a globally installed `staticcheck` was built with an older Go version.

## Test-driven development

TDD is required for feature work and bug fixes.

1. Write or update the failing test first.
2. Run the smallest relevant `go test ...` command and watch it fail for the expected reason.
3. Make the minimal code change.
4. Re-run the focused test.
5. Re-run the broader relevant package or repo gates.

Do not add production behavior without a failing test first unless the user explicitly approves an exception for docs-only work or other non-code changes.

## Testing expectations

- Keep tests next to the package they cover.
- Prefer `testing`, `httptest`, and `t.TempDir()`.
- Preserve offline default tests; default `go test ./...` must not require network access, real provider credentials, or a real interactive terminal.
- TTY-specific coverage must stay offline and automated, for example via the PTY smoke test in `cmd/otto/tui_pty_test.go`.
- Live provider tests are opt-in only. Gate them behind explicit environment variables and exclude them from the default suite.
- Contract changes need focused coverage for affected ownership, invalid states, cancellation, and history/wire compatibility. Verify both Session implementations where they share a contract; preserve unrelated behavioral assertions when migrating test fixtures.
- Report failing gates and reruns accurately. If unchanged code fails, check the baseline before assigning causality; never weaken validation or safety checks merely to obtain a pass.

## Secrets and safety

- Never add `--api-key`; API keys come only from environment variables, and ChatGPT credentials only from `otto login`.
- Never put raw API keys, OAuth tokens, or auth headers in TOML, JSONL session fixtures, logs, docs, or tests.
- Redact sample values in errors and examples.
- `read`, `grep`, `find`, `ls`, `write`, `edit`, and `skill` must reject workspace/skill-directory escapes after canonical path and symlink validation.
- Do not describe `bash` as always unsandboxed; Otto defaults to macOS Seatbelt and only explicit sandbox `off` is unsandboxed.
- Never put secrets in skill files; skill content is user- or repository-provided instruction text of the same class as `AGENTS.md` and `CLAUDE.md`.

## Documentation expectations

When updating docs:

- Keep README limited to implemented, tested behavior; list unsupported behavior under Limitations.
- Do not list roadmap stages or planned providers.
- Keep command examples aligned with the actual CLI flags in `cmd/otto/main.go`.
- Document the config/session/safety behavior that tests enforce today, not aspirational behavior.

## Commit guidance

Use small, focused commits with imperative subjects, for example:

- `feat: add OpenAI-compatible streaming`
- `docs: document the ChatGPT sign-in flow`

Before committing, run the relevant Go gates and confirm the working tree only contains intentional changes.
