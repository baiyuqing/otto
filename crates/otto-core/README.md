# otto-core

`otto-core` holds Otto's I/O-free contracts, wire formats and agent loop. It
uses no filesystem, network, process, environment or wall-clock API: the caller
supplies the clock, the identifier generator, the provider transport, the tool
executor and the session store. The implementations of these contracts for
native use (HTTP providers, file-backed sessions, built-in tools, sandbox) live
in [`crates/otto`](../otto) and the crates above it.

## Use as a git dependency

The crate is not published to crates.io (`publish = false`). Depend on a tag:

```toml
[dependencies]
otto-core = { git = "https://github.com/baiyuqing/otto", tag = "v0.5.0" }
```

Cargo finds the crate in the workspace by its package name, so no `package`
key or path is needed. `otto-core` declares dependency version ranges (for
example `regex = "1.13.1"` accepts any 1.x release from 1.13.1), so your
lockfile resolves the versions; the
workspace `Cargo.lock` pins only the `otto` binary. The minimum Rust version is
1.100.

## Modules

| Module | Contents |
| --- | --- |
| `agent` | The provider/tool turn loop: compaction, memory recall, inbox notifications, secret redactor |
| `approval` | Frontend-neutral approval requests, choices and lifecycle values |
| `claude` | I/O-free Claude Messages wire codec |
| `config` | Configuration schema, parsing and resolution without filesystem or environment access |
| `conformance` | Behavioural checks of the `Session` and `Mailbox` ports (feature `conformance`) |
| `diagnostics` | Content-free execution observations |
| `mailbox` | The FIFO mailbox contract for agent notifications |
| `memory` | Service-neutral long-term memory contracts |
| `model` | Transcript data model: roles, content blocks, messages, tool definitions, token usage |
| `openaicompat` | OpenAI-compatible Chat Completions wire codec |
| `openairesponses` | ChatGPT backend Responses API wire codec |
| `operation` | Operation-wide cancellation and deadline state |
| `provider` | The neutral `Provider` contract |
| `safetext` | Deterministic text canonicalization for security boundaries |
| `session` | The transcript store contract (`Session`) and an in-memory implementation (`MemorySession`) |
| `tool` | The `ToolExecutor` contract |
| `verification` | Validation of model-submitted verification verdicts |
| `wire` | The `otto serve` wire format and the reducers that read it |

## Example

`examples/scripted_agent.rs` runs one agent turn offline. A scripted `Provider`
requests an `add` tool call, a `ToolExecutor` runs it, and the provider then
returns a final text answer. The session is a `MemorySession`.

```sh
cargo run -p otto-core --example scripted_agent
```

## The `conformance` feature

The feature is enabled by default. It compiles `otto_core::conformance`, which
checks the behavioural rules of the `Session` and `Mailbox` ports against
instances you supply: message ordering, exactly-once input delivery, the
interrupted turn, the operation ledger, and mailbox FIFO, acknowledgement and
deduplication. Each failure names a stable check such as `session.input_id`.
Run these checks in the tests of your own `Session` or `Mailbox`
implementation. Disable default features to leave the module out.

## Stability

There is no API stability guarantee: any release of the repository, including
a patch release, may change the public API of `otto-core`. The crate version
follows the workspace version. Pin a tag and read the
[changelog](../../CHANGELOG.md) before moving to another one.
