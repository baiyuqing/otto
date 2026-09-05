# Repository Guidelines

Read this file first. It is the canonical short rulebook; the [development
guide](docs/development.md) contains the detailed package contracts and
workflow.

## Scope and task map

- Otto supports only the `openai-compatible` and `chatgpt` providers and runs on macOS.
- `cmd/otto` owns composition and process lifecycle; `internal/app` owns shared lifecycle and frontend capabilities; `internal/agent` owns provider/tool turns; `internal/provider` owns the neutral provider contract and its two implementation packages own wire formats.
- `internal/model`, `internal/session`, `internal/memory`, `internal/tool`, `internal/sandbox`, `internal/skill`, `internal/subagent`, `internal/repl`, `internal/tui`, and `internal/server` keep the responsibilities described in the [development guide](docs/development.md).
- The [architecture contract design](docs/specs/2026-09-05-architecture-contracts.md) records compatibility and ownership rationale. The [architecture import policy](internal/architecture/imports_test.go) enforces package direction.
- Current user behavior belongs in the [README](README.md) and [user manual](docs/user-manual.md). Design documents are historical rationale unless they explicitly say otherwise.

Do not document or implement other providers, and do not list planned
providers or roadmap stages in user-facing docs.

## Change rules

- Use a dedicated Git worktree and development branch for every feature or bug fix; never implement directly on `main`.
- If a design or spec is needed, get explicit approval before writing production code or tests. After approval, implement and verify the agreed scope without repeated confirmation; ask only for material scope or contract changes.
- TDD is required for feature work and bug fixes: failing test first, smallest RED run, minimal change, focused GREEN run, then broader relevant gates. Docs-only changes are exempt.
- The parent owns planning, interfaces, integration review, and acceptance. Delegated work has bounded file ownership; hand off overlapping file ownership and preserve other workers' changes.
- Keep dependencies and abstractions minimal, reuse existing contracts, and preserve append-only session history and compatibility. See the [development guide](docs/development.md) before changing shared contracts.

## Safety

- API keys come only from environment variables; ChatGPT credentials come only from `otto login`. Never place secrets in config, fixtures, logs, docs, tests, or skill files.
- File tools reject workspace and skill-directory escapes after canonical-path and symlink validation.
- `bash` runs through `internal/sandbox`; Seatbelt is the default and only explicit sandbox `off` is unsandboxed. It still starts in the selected workspace.
- Keep the default test suite offline: no provider credentials, network access, or real interactive terminal.

## Verification

Use the Makefile targets as the canonical commands:

```bash
make check-fast  # fmt, vet, architecture imports, focused core tests
make check       # full macOS build/lint/all-tests/race/PTY/diff gate
```

`make lint` uses pinned staticcheck v0.8.1. See the [development guide](docs/development.md)
for the focused package set, test commands, and contract-specific checks.

CI runs `make check` on pinned `macos-15` with Go 1.26.6 and pinned
`actions/checkout`/`actions/setup-go` SHAs. This documents the workflow
configuration; it does not claim that a remote run has passed.

Keep README and user-facing docs limited to implemented, tested behavior. Do
not describe `agent_send`, `agent_cancel`, `agent_report`, automatic memory
extraction, memory backup/restore/verify, `/skills`, `/skill`, or
`allowed-tools` enforcement as working features.
