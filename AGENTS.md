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
- `internal/memory`: neutral memory contracts, validation/secret guards, conservative policy, null service, and the shared Store conformance harness
- `internal/memory/sqlite`: secure local SQLite/FTS5 store and retriever implementing the memory contracts
- `internal/provider`: neutral provider contract
- `internal/provider/openaicompat`: all Stage 1 provider-specific HTTP/JSON/SSE code
- `internal/repl`: line-oriented REPL rendering and commands
- `internal/session`: in-memory and JSONL session storage
- `internal/tool`: workspace validation plus `read`/`grep`/`find`/`ls`/`write`/`edit`/`bash`
- `internal/tui`: full-screen Bubble Tea frontend, transcript rendering, Markdown/tool presentation, key handling, and terminal lifecycle

Rules:

- Keep provider-specific wire structs inside `internal/provider/openaicompat`.
- Keep file-tool workspace enforcement inside `internal/tool`.
- Keep session persistence append-only.
- Keep `bash` unsandboxed, but start it in the selected workspace.
- Keep `internal/memory` behind its neutral contracts; the agent loop, tools, and frontends must never reach a Store. The memory core is currently unwired — do not document it as a user-facing Stage 1 feature until config/tool/frontend wiring lands.

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
- `read`, `grep`, `find`, `ls`, `write`, and `edit` must reject workspace escapes after canonical path and symlink validation.
- Do not describe `bash` as sandboxed. It is intentionally unsandboxed.

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
