# Repository Guidelines

## Stage 1 scope

This branch implements Stage 1 only.

- Supported provider: `openai-compatible`
- Planned later: Codex subscription support, Claude subscription support
- Target platform: macOS

Do not document or implement Codex/Claude as working Stage 1 providers.

## Package boundaries

Keep responsibilities split along the current Go package layout:

- `cmd/otto`: CLI wiring, flags, process lifecycle, signal handling
- `internal/agent`: provider/tool orchestration and event emission
- `internal/app`: shared frontend/backend lifecycle, prompt serialization, session replacement, and exported session info/history access
- `internal/config`: TOML loading and runtime resolution
- `internal/model`: provider-neutral message/tool types
- `internal/memory`: neutral memory contracts, validation/secret guards, conservative policy, the `Service` implementation composing them with a `Store`/`Retriever`, a null service fallback, and the shared Store conformance harness
- `internal/memory/sqlite`: secure local SQLite/FTS5 store and retriever implementing the memory contracts
- `internal/provider`: neutral provider contract
- `internal/provider/openaicompat`: all Stage 1 provider-specific HTTP/JSON/SSE code
- `internal/repl`: line-oriented REPL rendering and commands
- `internal/sandbox`: sandbox driver contracts, environment filtering, and conformance helpers
- `internal/session`: in-memory and JSONL session storage
- `internal/skill`: SKILL.md frontmatter parsing, name/description validation, discovery across configured roots, and rendering of the system-prompt listing
- `internal/subagent`: child agent construction (`Runner`), task lifecycle, and the `agent`/`agent_wait`/`agent_status` tools
- `internal/tool`: workspace validation plus `read`/`grep`/`find`/`ls`/`write`/`edit`/`bash`/`skill`
- `internal/tui`: inline Bubble Tea frontend, transcript rendering, Markdown/tool presentation, key handling, and terminal lifecycle

Rules:

- Keep provider-specific wire structs inside `internal/provider/openaicompat`.
- Keep file-tool workspace enforcement inside `internal/tool`.
- Keep session persistence append-only.
- Keep `bash` delegated through `internal/sandbox`; only explicit sandbox `off` may use direct execution, and it still starts in the selected workspace.
- Keep `internal/memory` behind its neutral contracts; the agent loop, tools, and frontends must never reach a Store directly — only through `memory.Binding`/`memory.Reader`/`memory.Proposer` or the `app.Controller` memory facade. Per-turn recall and explicit management (`memory_search`/`remember`/`forget` tools, `/memory`/`/remember` in both frontends, `otto memory status|forget`) are wired end to end via `[memory]` TOML config; model- and human-originated writes always land as pending candidates requiring review. Automatic extraction (`Binding.Observe`) and durability (backup/restore/verify) remain unwired — do not document those as working Stage 1 features.
- Keep `internal/skill` free of imports from other Otto packages; the skill tool's file reads stay confined to the skill directory via `tool.Workspace`. Do not document `/skills`, `/skill`, or `allowed-tools` enforcement as working Stage 1 features.
- Keep `internal/subagent` behind `agent.New`: children are built only through `agent.New`; `internal/agent` knows tasks only through `agent.Tasks`/`agent.Inbox` and never imports `internal/subagent`; frontends reach tasks only through `app.TaskLister`; children never receive `agent*`, `remember`, `forget`, or `memory_search`; child transcripts are not persisted. Do not document named agent definitions, `[agents]` configuration, `context: inherit`, `agent_send`/`agent_cancel`/`agent_report`, the TUI task panel, or server task routes as working Stage 1 features.

## Working preferences

- Consider cost and efficiency when choosing models: default to the cheapest adequate model for routine work and escalate to a more capable model only when the task requires it.

## Development isolation

- Use a dedicated Git worktree and development branch for every feature or bug fix. Never implement directly on `main`.
- When work requires a design or spec, finish the discussion and get explicit approval before writing production code or tests.
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

## Secrets and safety

- Never add `--api-key`; Stage 1 uses environment variables only.
- Never put raw API keys, OAuth tokens, or auth headers in TOML, JSONL session fixtures, logs, docs, or tests.
- Redact sample values in errors and examples.
- `read`, `grep`, `find`, `ls`, `write`, `edit`, and `skill` must reject workspace/skill-directory escapes after canonical path and symlink validation.
- Do not describe `bash` as always unsandboxed; Stage 1 defaults to macOS Seatbelt and only explicit sandbox `off` is unsandboxed.
- Never put secrets in skill files; skill content is user- or repository-provided instruction text of the same class as `AGENTS.md` and `CLAUDE.md`.

## Documentation expectations

When updating docs:

- Keep README limited to Stage 1 claims.
- List Codex and Claude only as planned roadmap items.
- Keep command examples aligned with the actual CLI flags in `cmd/otto/main.go`.
- Document the config/session/safety behavior that tests enforce today, not aspirational behavior.

## Commit guidance

Use small, focused commits with imperative subjects, for example:

- `feat: add OpenAI-compatible streaming`
- `docs: document Otto Stage 1`

Before committing, run the relevant Go gates and confirm the working tree only contains intentional changes.
