# Lua plugin security contract

Lua plugin trust admits code, not host privileges: assume a package remains
malicious after explicit digest-bound trust. Protect files, processes, network,
credentials, Otto approval/tool authority, other calls and runtime availability.
Existing command plugins retain their separate native sandbox contract.

## Boundaries

Manifest/source → confined installer → digest snapshot/registry → handler
admission → fresh Lua VM → bounded JSON conversion → existing hook/tool response
validation. Core holds only portable manifest fields; native execution lives in
`crates/otto/src/plugin/hooks/{lua_handler,lua_engine}.rs`.

- Construct `Lua::new_with(StdLib::MATH, ...)`; only base and math computation
  exist. Remove base `load`, `loadfile`, `dofile`, `require`, `print`, `warn`,
  `collectgarbage`, protected calls and metatable access/mutation. No io, os,
  package, debug, FFI, coroutine, string-pattern or table-sort libraries. Removing
  protected calls prevents swallowing budget errors; preventing custom
  metatables also prevents Lua finalizers at VM destruction. No arbitrary
  require, module/native loading, tool dispatch or host userdata is provided.
- Only controlled JSON null/array construction crosses the host API boundary.
  Environment variables and package credential bindings never reach Lua. Payload
  strings are redacted using the existing hook/tool input path. Each call creates
  and destroys its own VM; no persistent state or module cache.
- Paths are package-relative `.lua`, without parent/absolute traversal; pinned
  root filesystem operations confine reads. Require regular UTF-8 text sources
  <=256 KiB and text-only VM loading; bytecode is rejected. Installation validates
  handlers. Exact snapshot digest/trust is checked before and after reading,
  during execution and before delivering results. This inherits the existing
  snapshot implementation and its same-user filesystem trust assumptions.
- Lua allocator limit: 16 MiB. Instruction budget: 2 million, checked every 1000
  instructions. Hooks also check an atomic cancellation flag and deadline.
  One absolute monotonic deadline is established before blocking-worker enqueue;
  queue delay consumes that budget. Worker entry/checkpoints and final delivery
  after trust/ownership checks reject expiry, including a completed result first
  polled late. Runtime cancellation, revoke or lost session ownership signals stop
  and joins the worker; dropping an already-polled invocation signals stop but
  cannot synchronously join from the destructor. The worker then exits cooperatively.
  A timeout alone is not termination. No unbounded native pattern/sort callbacks are exposed.
- Input: redacted JSON <=1 MiB, <=64 levels, <=8192 nodes. Output: raw traversal,
  <=64 levels, <=8192 nodes, bounded key/string bytes and final JSON <=64 KiB.
  Reject cycles/shared tables, sparse/mixed arrays, invalid UTF-8, non-finite
  numbers and non-JSON values. Error strings use fixed categories, never raw
  Lua errors, source, or arbitrary error-object formatting.
- Existing schemas, exact-argument approval and digest binding, response fields,
  hook deny-only authority, parent/child registration and non-replayability
  apply unchanged. Standalone TOML Lua hooks and memory Lua are unsupported.

## Cooperative limits, not OS isolation

The VM runs in-process. Native Lua/mlua vulnerabilities are residual risk; this
is **not an OS security boundary**. Lua allocator accounting excludes host and
engine overhead. Instruction hooks interrupt executing Lua, not arbitrary native
instructions: parsing, VM initialization/destruction, JSON conversion, bounded
base/math operations and snapshot hashing are not hard-preemptible. Source,
input, output, recursion and node limits bound those stages; checkpoints reject
results after cancellation/deadline, rather than promising a hard wall-clock
ceiling. Revocation observation is cooperative and snapshot hashing can add
latency. Strong hostile-native-code isolation needs a separate OS/process design.
Do not describe arbitrary code as reliably classifiable as malicious or safe.

## Regression and review requirements

`lua_engine_tests.rs` and `lua_handler_tests.rs` must assert capability absence,
malicious host-access attempts, infinite/memory-growth loops, cancellation and
expired deadlines (including small/invalid input and source), fresh VMs, bounded
JSON/cyclic/deep/sparse/mixed/large outputs and non-echoing errors. Integration
covers installation, inspect/trust/snapshot, schema validation, hooks/tools with
no command executor, revoke, source confinement and backward compatibility.
Review the actual stdlib allowlist, all Rust callbacks, cleanup behavior,
checkpoint/join path and serialization traversal independently before release.
Lifecycle regressions synchronize on an executing VM instruction hook before
cancellation, revoke, future drop and active writer-ownership loss, and observe
worker exit after VM destruction. Separate pre-entry cancellation tests do not
claim cancellation inside parsing, initialization, conversion or teardown: these
bounded native phases remain cooperative, not individually preemptible or
hard-real-time certified.

## Dependency/license review

`mlua = =0.12.2`, default features disabled, uses `lua54`, `vendored`, `serialize`
and `send`. Cargo.lock pins `mlua-sys 0.13.0` and `lua-src 551.0.2`, whose selected
Lua 5.4 source is 5.4.9. These crates declare MIT licensing; the mlua/lua-src registry
sources include LICENSE, and the selected Lua source retains its copyright notice.
The mlua-sys registry archive declares MIT in Cargo.toml (no separate LICENSE file). `luajit-src` is a vendored
build dependency in the lockfile but the selected engine is Lua 5.4, not LuaJIT.
Review these licenses and build-script/engine changes when upgrading. Preserve
upstream license notices with source redistribution; no interpreter install,
network service or module manager is needed at runtime.
