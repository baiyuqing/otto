# Code organization

This page is a map for contributors: where a behavior lives, which layer owns
it, and which entry point to start from. It complements the detailed contracts
in the [development guide](development.md); that guide is the canonical source
for invariants and implementation constraints.

Root `AGENTS.md` carries cross-repository rules. The Rust crates and `docs/`
have scoped `AGENTS.md` files; read the ones on the path being edited.
Detailed inventories and contracts stay on this page and in the development
guide rather than being copied into those instruction files.

## Repository map

| Path | Owns | Start here |
| --- | --- | --- |
| `crates/otto-core/` | WebAssembly-safe shared domain logic and protocol codecs | `src/lib.rs` |
| `crates/otto/` | Native Otto executable, runtime, persistence, integrations, and terminal/server frontends | `src/main.rs`, then `src/cli/` |
| `crates/otto-web/` | WebAssembly bindings that expose shared wire codecs to the browser | `src/lib.rs` |
| `ui/` | TypeScript browser frontend | `package.json` and `src/` |
| `connect/` | Independent Go Telegram and Feishu connector that communicates with Otto through ACP | `connect/README.md` and `connect/internal/` |
| `crates/otto/src/fsops.rs` | Shared pinned filesystem descriptors and atomic operations | Native session and workspace callers |
| `crates/otto/src/workspace.rs` | Persistent workspace identity, move history and state ownership | `initialize`, `read`, `fork` |
| `docs/` | User manual, contributor documentation, and historical design rationale | `user-manual.md`, `development.md`, and `specs/` |
| `testdata/` | Shared test fixtures and wire-contract inputs | The adjacent test that consumes a fixture |
| `scripts/` | Build and development helpers invoked by the Makefile and CI; `release.mjs` implements `make bump` and `make release-tag`, `ci-cache.mjs` reads tool versions from `Cargo.lock` and prunes the cached `target/` for the workflow, `license-check.mjs` implements `make license-check` | `Makefile`, `.github/workflows/checks.yml` |

The root Cargo workspace contains `otto-core`, `otto`, and `otto-web` only.
`connect/` has its own build definition and is not a root workspace crate.

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

- `approval`: frontend-neutral approval requests, choices and lifecycle values.
- `model`: provider-neutral messages, blocks, validation, and usage types.
- `provider`: the provider-neutral completion contract.
- `openaicompat` and `openairesponses`: provider wire formats and streaming
  codecs.
- `tool`: model-visible tool definitions and schema assembly.
- `session`: Pi v3 session codec, compaction, and context association types.
- `agent`: provider/tool turn orchestration and emitted events.
- `config`, `wire`, and `safetext`: shared configuration resolution, frontend
  DTOs, and secret-safe text handling.

`config/layered.rs` owns presence-preserving merge, field authority,
provenance and managed constraints. Native `config/layered.rs` discovers
workspace sources and content-bound executable trust; `cli/config_command.rs`
composes offline inspection/scoped editing, and `tool/project_guard.rs`
rechecks authorization at tool effects and requests cancellation on revocation.

Change shared contracts here only when both native and browser consumers need
them. Keep provider-specific transport and credential behavior out of this
crate.

### `crates/otto`: native application

`src/main.rs` starts the executable. `src/cli/` parses commands, runs the
REPL and owns process lifecycle; it is the primary starting point for a new
command or startup behavior. `src/runtime/` assembles one runnable agent from
resolved configuration (`builder.rs` holds `Builder` and `Runner`,
`wiring.rs` connects memory, MCP and sub-agents, `prompt.rs` builds the system
prompt). `src/app/` exposes the shared application lifecycle used by
frontends.

The frontends (`cli/`, `tui/`, `acp/`, `server/` and `client/`) build on every
other module, and no other module may import a frontend:
`crates/otto/tests/runtime_boundary.rs` fails when one does. See the
[crate split design](specs/2026-10-05-otto-crate-split.md).

The other modules group native responsibilities:

- **User entry points:** `cli/`, `tui/`, `server/`, and `acp/`.
- **Remote and external integrations:** `provider/`, `mcp/`, `auth/`, and
  `client/` (the client for a running `otto serve`).
- **Local state and reliability:** `approval/` (shared authority and domain adapters), `session/`, `config/`, `memory/`,
  `reflection/`, `typesafe/`, `storage.rs` (native Turso I/O), `usage.rs`, `failover/`, `workflow.rs`,
  and `schedule/` (scheduled deliveries: store, tools, worker).
- **Agent capabilities:** `tool/`, `sandbox/`, `skill/`, and `subagent/`.
- **Runtime assembly:** `runtime/`, and `metrics.rs` (the HTTP and turn
  metrics `server/` exposes and the schedule worker records).
- **Cross-cutting native helpers:** `deadline.rs`, `retry.rs`, `gourl.rs`,
  `urlprivacy.rs`, and `log.rs` (the process logger: one instance built at the
  composition root and passed to its consumers; event names and fields are in
  [observability](specs/2026-10-04-observability.md)).

Place native tool execution and workspace enforcement in `tool/`; `bash`
execution goes through `sandbox/`. Keep session persistence append-only.

### Browser and chat clients

- `crates/otto-web` is intentionally thin: it exposes `otto-core` wire codecs
  through `wasm-bindgen`.
- `ui/` calls the HTTP API provided by `crates/otto::server` and the WASM
  exports from `otto-web`; it does not contain Rust code.
- `connect/` does not link Otto code. It starts `otto acp` over stdio or
  connects directly to `otto serve` over authenticated loopback ACP TCP.
  Both use ACP v1; see [ACP TCP](specs/2026-10-03-acp-tcp.md).

## Common change paths

| If you are changing… | Start with… | Then check… |
| --- | --- | --- |
| A CLI command, flag, REPL command, or process lifecycle behavior | `crates/otto/src/cli/` | `app/` and the relevant frontend tests |
| Shared agent turn behavior, messages, tool schemas, or session encoding | `crates/otto-core/src/agent/`, `model.rs`, `tool.rs`, or `session/` | Native callers and WASM compatibility |
| Provider HTTP behavior | `crates/otto/src/provider/` | The matching wire module in `otto-core` |
| Shared approvals and model-initiated consent | `approval/`, `otto-core/src/approval.rs`, `tool/approval.rs` | Domain adapters, TUI, HTTP, ACP and chat connector |
| Agent self-help and pending approval dialogue | `tool/otto.rs`, `app/approval_control.rs` | `runtime/builder.rs`, ACP, HTTP, chat and TUI input paths |
| Active-task guidance and model follow-ups | `otto-core/src/agent/steering.rs`, `app/task_control.rs`, `tool/task_control.rs` | [Design and TODO](specs/2026-10-04-active-task-guidance.md), runtime tool/prompt wiring, HTTP/ACP and connector routing |
| Per-turn chat channel context | `app/channel.rs`, then ACP prompt parsing | Core per-run system context, HTTP turn queue, `client/`, and `connect/` |
| Reflection commands and background notices | `app/auto_reflection.rs`, `reflection/` | HTTP reflection routes, ACP commands, and chat updates |
| Model-requested reflection (`reflect` tool) and run logs | `tool/reflect.rs` (port), `app/requested_reflection.rs` (binding), `reflection/runlog.rs` (`/reflect list|show`) | [Design](specs/2026-10-05-reflect-tool.md), `cli/repl_commands.rs` (`reflect_runs_report`) |
| Web fetch and search tools, destination policy | `tool/web/` (`policy.rs` is the SSRF boundary) | [Design](specs/2026-10-04-web-fetch.md), `runtime/builder.rs` (`web_tools`), `reflection/taint.rs` |
| Native tool execution or sandbox behavior | `crates/otto/src/tool/` or `sandbox/` | Workspace and sandbox contract tests |
| Web API or browser behavior | `crates/otto/src/server/` or `ui/` | `crates/otto-web/` for shared browser wire codecs |
| ACP integrations | `crates/otto/src/acp/` | `client/` for attach mode; `connect/` for chat connector behavior |
| Wake-turn replies sent outside a prompt | `app/approval.rs` (`run_with_channel`), `acp/mod.rs` wake loop | [Design](specs/2026-10-04-acp-wake-replies.md), the `wake-replies` HTTP route, `client/`, `connect/internal/bridge` |
| Scheduled deliveries (`schedule` tools, due runs, delivery and write-back) | `crates/otto/src/schedule/`, `app/scheduled.rs` | [Design](specs/2026-10-04-scheduled-deliveries.md), turn origin in `app/approval.rs`, the worker spawn in `cli/run.rs` and `cli/serve.rs`, HTTP schedule routes, ACP `_otto/schedule/deliver`, `connect/internal/bridge`, `ui/` |
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
