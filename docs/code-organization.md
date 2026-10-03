# Code organization

This page is a map for contributors: where a behavior lives, which layer owns
it, and which entry point to start from. It complements the detailed contracts
in the [development guide](development.md); that guide is the canonical source
for invariants and implementation constraints.

## Repository map

| Path | Owns | Start here |
| --- | --- | --- |
| `crates/otto-core/` | WebAssembly-safe shared domain logic and protocol codecs | `src/lib.rs` |
| `crates/otto/` | Native Otto executable, runtime, persistence, integrations, and terminal/server frontends | `src/main.rs`, then `src/cli/` |
| `crates/otto-web/` | WebAssembly bindings that expose shared wire codecs to the browser | `src/lib.rs` |
| `ui/` | TypeScript browser frontend | `package.json` and `src/` |
| `connect/` | Independent Go Telegram and Feishu connector that communicates with Otto through ACP | `connect/README.md` and `connect/internal/` |
| `desktop/` | Independent macOS Tauri shell around `otto serve` | `desktop/README.md` |
| `docs/` | User manual, contributor documentation, and historical design rationale | `user-manual.md`, `development.md`, and `specs/` |
| `testdata/` | Shared test fixtures and wire-contract inputs | The adjacent test that consumes a fixture |
| `scripts/` | Build and development helpers invoked by the Makefile | `Makefile` |

The root Cargo workspace contains `otto-core`, `otto`, and `otto-web` only.
`connect/` and `desktop/` have their own build definitions and are not root
workspace crates.

## Rust layers

```text
CLI / TUI / HTTP server / ACP
              │
              ▼
       crates/otto: app and native services
              │
              ▼
 crates/otto-core: shared contracts and agent loop
              ▲
              │
crates/otto-web: browser-facing WASM bindings
              ▲
              │
        ui/: TypeScript browser client
```

`otto-core` must remain buildable for `wasm32-unknown-unknown`. Native-only
concerns—filesystem access, network transports, process control, Turso,
authentication, and the sandbox—belong in `otto`, not in `otto-core`.
`make rust-wasm-check` enforces this boundary.

### `crates/otto-core`: shared, WASM-safe logic

`crates/otto-core` contains no native runtime integration. Its principal
modules are:

- `model`: provider-neutral messages, blocks, validation, and usage types.
- `provider`: the provider-neutral completion contract.
- `openaicompat` and `openairesponses`: provider wire formats and streaming
  codecs.
- `tool`: model-visible tool definitions and schema assembly.
- `session`: Pi v3 session codec, compaction, and context association types.
- `agent`: provider/tool turn orchestration and emitted events.
- `config`, `wire`, and `safetext`: shared configuration resolution, frontend
  DTOs, and secret-safe text handling.

Change shared contracts here only when both native and browser consumers need
them. Keep provider-specific transport and credential behavior out of this
crate.

### `crates/otto`: native application

`src/main.rs` starts the executable. `src/cli/` parses commands and composes
concrete dependencies; it is the primary starting point for a new command or
startup behavior. `src/app/` exposes the shared application lifecycle used by
frontends.

The other modules group native responsibilities:

- **User entry points:** `cli/`, `tui/`, `server/`, and `acp/`.
- **Remote and external integrations:** `provider/`, `mcp/`, `auth/`, and
  `client/` (the client for a running `otto serve`).
- **Local state and reliability:** `session/`, `config/`, `memory/`,
  `reflection/`, `storage.rs` (native Turso I/O), `usage.rs`, `failover/`, and `workflow.rs`.
- **Agent capabilities:** `tool/`, `sandbox/`, `skill/`, and `subagent/`.
- **Cross-cutting native helpers:** `deadline.rs`, `retry.rs`, `gourl.rs`, and
  `urlprivacy.rs`.

Place native tool execution and workspace enforcement in `tool/`; `bash`
execution goes through `sandbox/`. Keep session persistence append-only.

### Browser, chat, and desktop clients

- `crates/otto-web` is intentionally thin: it exposes `otto-core` wire codecs
  through `wasm-bindgen`.
- `ui/` calls the HTTP API provided by `crates/otto::server` and the WASM
  exports from `otto-web`; it does not contain Rust code.
- `connect/` does not link Otto code. It starts or connects to `otto acp` and
  uses ACP v1 on stdio as its contract.
- `desktop/` wraps `otto serve` in a macOS Tauri application and remains a
  separate Cargo workspace.

## Common change paths

| If you are changing… | Start with… | Then check… |
| --- | --- | --- |
| A CLI command, flag, REPL command, or process lifecycle behavior | `crates/otto/src/cli/` | `app/` and the relevant frontend tests |
| Shared agent turn behavior, messages, tool schemas, or session encoding | `crates/otto-core/src/agent/`, `model.rs`, `tool.rs`, or `session/` | Native callers and WASM compatibility |
| Provider HTTP behavior | `crates/otto/src/provider/` | The matching wire module in `otto-core` |
| Native tool execution or sandbox behavior | `crates/otto/src/tool/` or `sandbox/` | Workspace and sandbox contract tests |
| Web API or browser behavior | `crates/otto/src/server/` or `ui/` | `crates/otto-web/` for shared browser wire codecs |
| ACP integrations | `crates/otto/src/acp/` | `client/` for attach mode; `connect/` for chat connector behavior |
| Persistent memories, sessions, workflow runs, or token aggregates | The matching native state module | Its store and lifecycle tests |

## Dependency and documentation rules

- Keep dependencies pointed toward shared contracts: frontends and native
  integrations depend on `otto-core`; `otto-core` does not depend on native
  services or browser UI code.
- Keep HTTP/SSE DTOs in `crates/otto/src/server` separate from internal types;
  share only the wire types that genuinely cross frontend boundaries.
- Treat `docs/specs/` as design rationale unless a document says otherwise.
  Current behavior belongs in the README and user manual; implementation
  contracts belong in the development guide.
- When moving a module or changing ownership, update this map, `AGENTS.md`,
  and the relevant detailed contract together.

## Verification

For documentation-only changes, check links and review the rendered Markdown.
For code organization changes, use the relevant focused tests and then the
canonical repository gates described in the [development guide](development.md).
