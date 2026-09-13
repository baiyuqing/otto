# Rust rewrite plan

Status: done. All phases below landed; the Go implementation was removed at
tag `go-final`. This document is historical rationale; current behavior is
described in [AGENTS.md](../../AGENTS.md), the [development
guide](../development.md), the [README](../../README.md), and the [user
manual](../user-manual.md).

Goal: replace the Go implementation of Otto with Rust, keep every user-visible
behavior and on-disk format, and compile the provider-neutral core to
WebAssembly so the browser UI and the native binary share one implementation
of wire types, stream parsing, and transcript state.

## Baseline

| Item | Value |
|---|---|
| Go production code | 49,053 lines in 19 `internal` packages plus `cmd/otto` |
| Go test code | 75,734 lines |
| Direct Go dependencies | 11 |
| Browser UI | 1,442 lines TypeScript, unaffected except `sse.ts`, `transcript.ts`, `types.ts` |
| Platform | macOS only, Seatbelt via `/usr/bin/sandbox-exec` |
| Rust toolchain on host | cargo 1.98.0, target `aarch64-apple-darwin`; `wasm32-unknown-unknown` not installed |

Contracts that must survive unchanged:

- Session files: Pi v3 JSONL, append-only, `otto.runtime` custom entries,
  16 MiB entry cap, 256 MiB file cap. Fixtures live in
  `internal/session/testdata`.
- Config: `~/.config/otto/config.toml`, unknown fields rejected, API keys only
  from environment variables, ChatGPT credentials only from
  `~/.otto/auth/chatgpt.json`.
- HTTP API: `internal/server/openapi.yaml`, bearer-token gating of `/v1/`,
  Unix-socket and loopback-TCP listeners, `/metrics` text format.
- Sandbox: `internal/sandbox/seatbelt/profile_v1.sb` byte for byte, private
  HOME/TMPDIR/cache directories with mode 0700, environment filtering rules
  in `internal/sandbox/environment.go`.
- Tool schemas and names: `read`, `grep`, `find`, `ls`, `write`, `edit`,
  `bash`, `skill`, `memory_search`, `remember`, `forget`, `agent`,
  `agent_wait`, `agent_status`.
- Secret redaction: provider error bodies, environment values, and stream
  text never contain the API key or private directory paths.

## Decisions

Each item below is the proposed default. Change it before phase 0 starts.

1. **WebAssembly boundary.** The crate `otto-core` has no filesystem, network,
   process, or clock dependency and must build for `wasm32-unknown-unknown`.
   It holds the message model, provider request and response types, the
   OpenAI-compatible and Responses wire translation, the SSE frame parser,
   the Pi v3 entry codec, agent event types, the transcript reducer, and
   config resolution over already-parsed input. The browser UI loads
   `otto-core` through `wasm-bindgen` and stops maintaining its own
   `sse.ts`, `transcript.ts`, and `types.ts`. The `wasm32` build is the
   architecture guard: it fails if anyone adds an OS dependency to the core,
   the same role `internal/architecture/imports_test.go` plays today.

   Not planned: running tools or bash in WebAssembly, and a WebAssembly
   plugin runtime for skills. Both add a second sandbox model next to
   Seatbelt without a current user need.

2. **Crate layout.** Three crates in a Cargo workspace at the repository root.

   | Crate | Kind | Contents |
   |---|---|---|
   | `crates/otto-core` | lib, wasm-safe | model, provider contract, wire codecs, SSE parser, session entry codec, agent loop, events, transcript reducer, config resolution |
   | `crates/otto` | bin | CLI, config file I/O, HTTP transport, JSONL store, tools, workspace validation, sandbox, auth, memory, skills, subagents, app controller, REPL, TUI, server, embedded web UI |
   | `crates/otto-web` | cdylib, wasm | `wasm-bindgen` exports over `otto-core` for the browser |

   The Go code separates REPL, TUI, and server into packages. In Rust these
   are modules inside `crates/otto`; a crate per frontend adds build
   configuration without changing any dependency direction.

3. **Coexistence.** Rust lands on `main` under `crates/` through ordinary
   feature branches and worktrees, one pull request per phase. The Go binary
   stays the shipped binary until phase 9. Nothing in `crates/` touches the Go
   build, so `make check` is unaffected until the switch.

4. **Async and cancellation.** `tokio` in `crates/otto`. In `otto-core` the
   agent loop is `async` and generic over a `Provider` trait and a
   `ToolExecutor` trait; it takes a `CancellationToken` from `tokio-util`
   (which builds on `wasm32` without the `rt` feature). Go's
   `context.WithoutCancel` for tool-result persistence becomes an explicit
   non-cancellable append call on the session trait.

5. **JSON.** `serde` and `serde_json`. Go's `json.RawMessage` becomes
   `Box<serde_json::value::RawValue>`. Tool parameter schemas stay as raw
   JSON bytes end to end, which removes the `json.Number` special case in
   `model.ToolDefinition.UnmarshalJSON`.

6. **Ownership rules from the Go contracts.** Go documents "borrowed
   read-only request payloads" and "caller-owned returned data" in comments
   and enforces them with clone helpers and ownership tests. In Rust these
   become signatures: `&Request` for borrowed input, owned `Response` for
   output, `Arc<dyn Provider>` for sharing between parent and child agents.
   The `CloneMessage`/`CloneMessages`/`CloneUsage` helpers and their tests
   are not ported; `#[derive(Clone)]` covers them.

## Dependency map

| Go | Rust | Note |
|---|---|---|
| `encoding/json` | `serde_json` | `RawValue` for pass-through |
| `pelletier/go-toml` | `toml` | strict unknown-field rejection via `serde(deny_unknown_fields)` |
| `regexp` | `regex` | same RE2-class semantics, no backtracking |
| `net/http` client | `reqwest` with `rustls` | dial, TLS, and header timeouts set per phase 4 |
| `net/http` server | `axum` on `hyper` | Unix socket via `tokio::net::UnixListener` |
| `bubbletea`, `lipgloss`, `bubbles` | `ratatui`, `crossterm` | |
| `glamour` (terminal Markdown) | `pulldown-cmark` plus an in-repo renderer to `ratatui` spans | no maintained equivalent; largest unported piece of the TUI |
| `creack/pty` (tests only) | `nix::pty::openpty` | PTY smoke test only |
| `golang.org/x/oauth2` | hand-written PKCE flow over `reqwest` | Go uses a small surface of the library |
| `modernc.org/sqlite` | `rusqlite` with `bundled` and `fts5` features | |
| `os/exec`, `syscall` | `tokio::process`, `nix` | process groups, `kill`, `Stat_t` uid checks |
| `context.Context` | `CancellationToken` and future drop | |
| `crypto/rand`, `sha256` | `rand`, `sha2` | |
| `golang.org/x/term` | `crossterm` | terminal detection |
| flag | `clap` | flag names and help text kept identical |
| WebAssembly bridge | `wasm-bindgen`, `serde-wasm-bindgen`, `wasm-bindgen-test` | |

Test-only: a local `hyper` server bound to `127.0.0.1:0` replaces
`httptest`. No test may reach the network.

## Phases

Each phase is one pull request from its own worktree, with tests ported
first and Go tests left running unchanged. Line counts are the Go source
being replaced and are the sizing basis, not a target.

### Phase 0: toolchain and boundary spike

- Add `Cargo.toml` workspace, `rust-toolchain.toml` pinning the channel,
  `rustfmt.toml`, `clippy` configuration, and the three crate skeletons.
- Install target `wasm32-unknown-unknown`.
- Prove the boundary with the smallest end-to-end program: a fake `Provider`,
  a fake `ToolExecutor`, and the agent loop skeleton running one text turn
  and one tool turn, tested under native `tokio` and under
  `wasm-bindgen-test`.
- Makefile targets: `rust-fmt`, `rust-lint` (clippy with `-D warnings`),
  `rust-test`, `rust-wasm-check` (`cargo check -p otto-core --target
  wasm32-unknown-unknown`). CI adds a job running them; `make check` is
  unchanged.

Exit criteria: all four targets pass in CI. This phase settles the async,
cancellation, and JSON crate choices; if `tokio-util` fails on `wasm32`, the
loop takes a `&dyn Fn() -> bool` cancellation check instead.

### Phase 1: otto-core model and OpenAI-compatible codec (1,174 Go lines)

- `model`: `Role`, `BlockType`, `Block`, `Message`, `ContextMetadata`,
  `ToolDefinition`, `FinishReason`, `Usage` with the validation rules in
  `internal/model/types.go` ported as table tests from `types_test.go`.
- `provider`: `Request`, `Response`, `StreamEvent`, `ContextOverflowError`,
  the `Provider` trait, `RequestSizer`.
- `openaicompat` wire: request translation, the SSE stream assembler in
  `stream.go`, `finishReason`, the overflow classifier in `overflow.go`
  including the duplicate-key rejection. Tests ported from
  `stream_test.go`, `overflow_test.go`, `security_test.go`.
- HTTP is not in this phase; the parser takes `&[u8]` or an
  `AsyncBufRead`.

### Phase 2: session codec and JSONL store (4,911 Go lines)

- `otto-core`: Pi v3 entry types and the codec in `pi_types.go`,
  `pi_codec.go`, `pi_details.go`, including legacy usage normalization and
  the explicit-zero usage marker.
- `crates/otto`: the `Store` in `store.go` with `O_CREAT|O_EXCL` creation,
  0700 directories, 0600 files, `fsync` after each record, lazy file
  creation, entry and file size caps, `ErrFatalPersistence` poisoning, plus
  `archive.go`, `list.go`, `snapshot.go`, `prepared.go`, `compaction.go`.
- Interop gate: Rust reads every file under `internal/session/testdata`
  and produces the same message sequence as the Go store; a Rust test writes
  a session, and a Go test under `make rust-interop` opens it and asserts
  equality. Both directions must pass before the phase merges.

### Phase 3: tools, workspace, sandbox (3,653 Go lines)

- `Workspace`: canonical path plus symlink validation; every escape case in
  `internal/tool/security_test.go` and `workspace_test.go` ported.
- `read`, `grep`, `find`, `ls`, `write`, `edit` (including the
  fuzzy-whitespace matching and diff output added in #90), `skill`,
  output caps, `PersistedContent` semantics.
- Sandbox: `Executor`, request validation, environment classification and
  private directories from `environment.go`, the Seatbelt driver with
  self-test probes, the direct driver, process group management from
  `nativeprocess`. `profile_v1.sb` is moved to a shared path and included
  with `include_str!`.
- `bash` tool over the executor.

### Phase 4: provider transport, config, agent loop, REPL, headless mode (4,300 Go lines)

- `reqwest` client with the timeouts and same-origin redirect policy from
  `client.go`, retry with `Retry-After`, error-body cap, key redaction.
- Config: `File` with `deny_unknown_fields`, `Load`, `Save`,
  `SetDefaultProfile` text replacement, `Resolve`, `ResolveSandbox`,
  `model_limits.go`. Resolution logic lives in `otto-core`; file I/O in
  `crates/otto`.
- Agent: `Run`, `compaction.go`, `compaction_select.go`,
  `context_estimate.go`, `overflow.go`, `redactor.go`, `summary*.go`,
  `inbox.go`, `tasks.go`. Memory recall is a trait with a no-op
  implementation until phase 7.
- `crates/otto` binary: `clap` flags matching `cmd/otto/main.go`, system
  prompt assembly, workspace instruction file, REPL from `internal/repl`,
  `--approve` one-shot mode, `--continue`, `--resume`, `--archive`.

Exit criteria: the Rust binary completes a coding task against an
OpenAI-compatible endpoint with Seatbelt on, and the session file it writes
resumes in the Go binary.

### Phase 5: ChatGPT provider (1,449 Go lines)

- `auth`: PKCE sign-in, credential file at `~/.otto/auth/chatgpt.json`
  with the same JSON shape, refresh, `otto login` and `otto logout`.
- `openairesponses`: Responses API request translation and SSE decoding.

### Phase 6: app controller, server, web UI bridge (3,940 Go lines)

- `app::Controller`: operation admission, session replacement, profile
  switching, close semantics, `Info`, task views, wake preparation.
- Server: routes from `openapi.yaml`, per-session turn buffering, SSE
  events, token gating, listeners, metrics text output, sandbox reload,
  embedded UI via `include_dir`.
- `otto-web`: exports for SSE frame parsing, the transcript reducer, and
  event types. `ui/` replaces `sse.ts`, `transcript.ts`, and `types.ts`
  with the wasm package; `make ui` runs `wasm-pack build` before `vite
  build`. Existing `vitest` cases become the acceptance tests for the wasm
  exports.

### Phase 7: memory, skills, subagents (5,450 Go lines)

- Memory contracts, policy, secret guards, `Service`, null service, the
  `rusqlite` FTS5 store with the same schema and file location, the store
  conformance harness, `memory_search`/`remember`/`forget` tools,
  `/memory` and `/remember` commands, `otto memory status|forget`.
- Skills: `SKILL.md` frontmatter parsing, discovery, prompt section.
- Subagents: `Runner`, task lifecycle, `agent`/`agent_wait`/`agent_status`,
  `AGENT.md` definitions, `context: inherit`.

### Phase 8: TUI (5,650 Go lines)

- `ratatui` application on the alternate screen: transcript entries,
  layout, key map, commands, completion, compaction, login, memory,
  profile, rename, resume, sandbox, and task panes.
- Markdown renderer over `pulldown-cmark`. This is the only component with
  no library equivalent; budget it separately.
- PTY smoke test with `nix::pty`, matching `cmd/otto/tui_pty_test.go`.

### Phase 9: parity gate and switch

- `make check` runs the Rust gates; `make build` produces the Rust binary.
- README, `README.zh-CN.md`, user manual, development guide, `AGENTS.md`,
  and the CI workflow updated in the same change.
- Tag the last Go commit `go-final`, delete `cmd/`, `internal/`, `go.mod`,
  `go.sum`, and the Go CI job.

## Test migration policy

- Every Go test file is read before its package is ported. Behavioral
  assertions are ported as Rust table tests; assertions that only check Go
  deep-copy or nil-interface behavior are dropped and listed in the phase
  pull request.
- Shared fixtures (`internal/session/testdata`, `profile_v1.sb`, skill and
  agent definition samples) move to `testdata/` at the repository root in
  phase 2 and are referenced from both languages until phase 9.
- The Go suite keeps passing unchanged through phase 8. A Rust change may
  not edit Go tests except to add the interop checks.
- Default Rust tests are offline, need no credentials, and use no real
  terminal, matching the Go rule.

## Risks

| Risk | Effect | Mitigation |
|---|---|---|
| Session format divergence | Users lose history on switch | Two-way interop gate in phase 2, kept in CI until phase 9 |
| Seatbelt self-test behavior differs under a Rust-spawned process | `bash` unavailable or under-confined | Port the probe set unchanged; run the existing `security_integration_test` scenarios against the Rust binary |
| Terminal Markdown renderer | TUI phase overruns | Ship the TUI with plain-text rendering first; Markdown renderer is a separate pull request |
| `tokio-util` or `wasm-bindgen` version constraints on `wasm32` | Core boundary needs a different cancellation type | Settled in phase 0 before any port |
| Single maintainer | Phases 1 to 4 block all Go feature work | Go stays shipped; Go fixes continue on `main` and are re-ported into the phase touching that package |

## Rust concepts by phase

For the learning goal, each phase exercises a distinct area:

- Phase 0: workspaces, features, conditional compilation, `wasm-bindgen`.
- Phase 1: `serde` derive and custom deserializers, enums with data,
  `Result` and `thiserror`.
- Phase 2: file descriptors, `O_EXCL`, `fsync`, mutex-protected state,
  error poisoning.
- Phase 3: `std::path` canonicalization, `nix` process groups, `unsafe`
  boundaries around `fork`/`exec`, trait objects for drivers.
- Phase 4: `async`/`await`, `tokio` runtime, `reqwest` streaming bodies,
  `CancellationToken`, generic agent loop over traits.
- Phase 6: `axum` handlers, SSE streams, `Arc`/`RwLock` sharing,
  `serde-wasm-bindgen` at the JavaScript boundary.
- Phase 7: `rusqlite` and FTS5, prepared statements, transactions.
- Phase 8: `ratatui` immediate-mode rendering, terminal raw mode, event
  loops.

## Not in scope

- New providers, new tools, or behavior changes of any kind before phase 9.
- WebAssembly execution of tools, bash, or skills.
- Linux or Windows support.
