# Main/Sandbox Branch Integration Design

**Date:** 2026-09-01  
**Status:** Proposed  
**Target:** merge `origin/main` at `2def1babb88e86073d60e4ad3f90ce0be34b9f4c` into `design/sandbox-driver`

## Goal

Make PR #44 mergeable without rewriting its 28-commit history. The integrated branch must preserve all behavior currently on `main`—session archiving, wired Memory management and recall, profile selection/default persistence, current TUI presentation, and the separately identified Stage 2 ChatGPT-subscription provider—while retaining the approved macOS Sandbox security and lifecycle invariants.

This is a semantic merge, not a choice of one side's complete files. `main` and the Sandbox branch both changed startup, runtime construction, Agent, Controller, configuration, TUI layout, tests, and documentation. Conflict resolution must compose those responsibilities.

## Merge strategy

Create a normal merge commit from `origin/main` into `design/sandbox-driver`; do not rebase, squash, or force-push.

Authority by concern:

- Current `main` owns features merged after the Sandbox fork: archive, wired Memory, profile switching/default persistence, current TUI behavior, auth/provider additions, and their tests/docs.
- The Sandbox branch owns command execution, immutable environment capture, secret-completeness handling, Bash registration, Seatbelt/Direct selection, sanitized Sandbox status, process cleanup, and Sandbox documentation.
- Existing approved Memory and Sandbox contracts remain binding. Neither subsystem may import or directly operate the other's backend.

## Integration behavior

### Startup and process lifecycle

Normal agent startup captures the process environment once. HOME, config/provider lookup, Sandbox policy, shell resolution, and child environment derive from that immutable snapshot; no conflict resolution may restore live `os.Getenv`, `os.Environ`, or `os.UserHomeDir` reads in the normal path.

The normal path then:

1. parses fixed CLI syntax and dispatches existing standalone commands;
2. loads config and resolves runtime, Memory, workspace, session, and Sandbox settings;
3. establishes interrupt ownership before opening Seatbelt;
4. opens the process Memory service and process Sandbox runtime;
5. activates or creates a session only after the confidentiality boundary is known complete;
6. builds one Controller using all applicable Memory, archive, profile, session-browser, runtime-status, and dynamic-content options.

Process shutdown remains cancellation first. Controller closure drains runners and releases Memory bindings; Sandbox closure then drains command processes and private state; the process Memory service closes only after bindings are released. Existing fixed error precedence and idempotent cleanup rules remain intact.

Standalone login/logout/memory/archive paths retain their existing early-exit behavior, but path-bearing diagnostics and stored credentials must remain bounded and sanitized.

### Runtime builder and tools

The merged `runtimeBuilder` contains both sets of immutable process dependencies:

- config path/file, runtime overrides, workspace/session factories, profile persistence, and Memory service/scopes/budgets from `main`;
- one shared Sandbox Executor, child environment, sanitized Sandbox status, retained secret set, and completeness capability from PR #44.

Every runner receives the six workspace file tools. Bash is added only through `tool.NewBashTool` when the process Sandbox boundary is usable. Memory tools are added only when Memory is usable, through neutral `Reader`/`Proposer`/service surfaces. `/new`, `/resume`, and profile replacement reuse the exact process Executor/environment/status and create a fresh per-runner Memory binding; runners close bindings but never the process Executor or Memory service.

Provider construction remains provider-specific in provider packages. Its result must expose or internally protect every credential required by the confidentiality boundary. OpenAI-compatible API keys/endpoints and any loaded OAuth access/refresh/identity/account values must never enter Bash, Agent-visible errors/events, frontend state, or Pi JSONL. Credential rotation must not make an untracked value observable; provider packages must sanitize their own wire/auth failures, and process redaction must include all stable credential forms available at construction.

### Agent and dynamic-content suppression

Agent construction composes Memory options with the Sandbox branch's boundary redactor. A complete turn may recall request-local Memory before the provider call; recalled text remains absent from sessions and compaction.

If secret collection is incomplete, a safe marker is unrepresentable, or a retained secret collides with mandatory runtime/control fields, the process enters dynamic-content suppression before session activation or provider/Memory/tool work. In that state:

- no provider, Memory recall, Memory proposal, Bash, compaction, archive, profile switch, session replacement, or persistent mutation occurs;
- no dynamic history, session metadata, usage, runtime identity, Memory result, or provider result crosses a frontend boundary;
- only fixed lifecycle/status output is allowed.

Controller gating therefore applies not only to existing Info/History/New/Resume/List methods, but also to merged archive, profile, and Memory facades.

### Configuration and presentation

`config.File` and resolution retain both `[memory]` and `[sandbox]` schemas plus current provider/profile behavior. Sandbox remains process-wide and immutable across session/profile replacement. Profile default persistence must not rewrite or discard `[memory]` or `[sandbox]` TOML sections.

TUI/REPL integration preserves current archive, Memory, profile, and usage UI while adding the sanitized Sandbox status/warning. Narrow layouts wrap rather than omit security status. Bubble Tea presentation state still changes only in `Update`.

### Documentation

Resolve `README.md`, `docs/user-manual.md`, and `AGENTS.md` semantically:

- preserve current `main` documentation for archive, wired Memory, current profile/UI behavior, and separately labeled Stage 2 subscription support;
- replace all stale “Bash is always unsandboxed” statements with the implemented macOS `auto -> seatbelt`, fail-closed, and explicit `off` behavior;
- retain the Sandbox config, threat limitations, remediation, environment/read/network details, and Driver authoring link from PR #44;
- describe no Docker or Apple Container backend as working;
- preserve `CLAUDE.md` unchanged except for changes already present on `main`.

## Conflict-resolution tests

Conflict resolution begins with the merge's compile/test failures as integration RED evidence. Add focused tests only where existing tests do not prove composition:

- startup with Memory enabled and Seatbelt available exposes Memory tools plus Sandbox-backed Bash while sharing one Executor;
- unavailable/incomplete Sandbox retains six file tools and suppresses every dynamic Memory/session/profile/archive/provider boundary;
- `/new`, `/resume`, and profile switching reuse Sandbox identity and create/close distinct Memory bindings;
- Controller close drains runner bindings before Sandbox and process Memory service cleanup;
- OpenAI-compatible and subscription credentials cannot enter child environment, provider-visible follow-ups, frontend events, or Pi JSONL;
- config default-profile persistence preserves `[memory]` and `[sandbox]` sections;
- TUI/REPL/headless status continues to display Sandbox state alongside current Memory/profile/archive behavior;
- README and user manual retain both current-main features and exact Sandbox behavior.

Run focused package tests while resolving, then the complete offline normal/race/PTY/Seatbelt/build/vet/pinned-Staticcheck/source gates. Push the merge commit normally to update PR #44.

## Non-goals

- No rebase or force-push.
- No change to Memory authority, persistence, recall, or extraction policy.
- No change to archive or profile-switch semantics.
- No new provider, auth, container, remote-execution, or Sandbox backend behavior.
- No weakening of fail-closed redaction, process cleanup, file-tool workspace checks, or append-only session persistence.
