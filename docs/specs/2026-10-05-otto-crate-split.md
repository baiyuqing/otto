# Splitting `crates/otto` into layered crates

Status: approved on 2026-10-05 for phase 1 and the separate clippy job;
phases 2 to 4 are decided after phase 1 is measured. Phase 1 is implemented:
the runtime assembly moved from `cli` to `crates/otto/src/runtime/`,
`server::metrics` became `crate::metrics`, and
`crates/otto/tests/runtime_boundary.rs` guards the boundary.
Base: `164968b` (main after #331).

## Why

After #330, a warm CI run spends most of its time compiling this
workspace's own code, and almost all of that is `crates/otto`:

| Run #584 (warm cache) | macOS | Linux |
|---|---|---|
| `cargo clippy --workspace --all-targets` | 2m00s | 1m10s |
| `cargo test --workspace` build | 4m44s | 2m30s |
| Whole job | 11m38s | 5m49s |

`crates/otto` holds 163k lines: 92k of production code and 71k of tests.
Rust compiles a crate's frontend largely on one thread, so all of it sits on
one critical path. On a 4-core machine with dependencies built,
`cargo test -p otto --no-run --timings` reports:

- 240s for the `otto` lib. Every integration test and the binary wait for
  it.
- 140s for the `otto` lib test target, which runs alongside the lib.
- about 20s for the 20 integration test binaries, which start after the lib.

`-Ztime-passes` on the lib, built alone (134s of rustc time):

| Pass | Time |
|---|---|
| `monomorphization_collector_graph_walk` | 61s |
| `MIR_borrow_checking` | 52s |
| `type_check_crate` | 9s |
| `codegen_crate` + `LLVM_passes` | 14s |

Code generation is about 10% of the time. Most of it is spent in analysis and
in monomorphization collection. The metadata that dependent crates wait for
(`generate_crate_metadata`, 62s) is not written until monomorphization
collection has finished. So Cargo's pipelining, which starts a dependent
crate on its dependency's metadata, gains little for this crate. Splitting
helps only where separate crates can be analysed at the same time.

## What the module graph allows

Counting only production code (`#[cfg(test)]` items and test files left
out), the modules form:

- 9 acyclic leaves: `auth`, `deadline`, `log`, `memory`, `provider`,
  `retry`, `storage`, `typesafe`, `usage`, plus `gourl` and `urlprivacy`,
  which refer to each other. Together: 12.2k production lines and 6.8k test
  lines.
- One strongly connected component of 18 modules: `acp`, `app`, `approval`,
  `cli`, `client`, `config`, `failover`, `mcp`, `reflection`, `sandbox`,
  `schedule`, `server`, `session`, `skill`, `subagent`, `tool`, `tui`,
  `workflow`.

Most of the edges that tie the runtime to the frontends point to code that is
in the wrong module, not to real coupling:

| Edge | What is used | Fix |
|---|---|---|
| `app`, `tool`, `reflection` → `cli` | `cli::runtime_builder` (`Runner`, `Builder`, `RuntimeInfo`, `SharedSession`), `cli::wiring`, `cli::sandbox_setup`, `cli::info` | Move these 7.1k lines, which assemble the runtime, out of `cli` and into the runtime layer |
| `config` → `cli` | `runtime_builder::random_id` | Moves with `runtime_builder` |
| `client` → `cli` | `info::{SandboxInfo, SandboxMode, SandboxNetwork}` | Moves with `info` |
| `schedule` → `server` | `server::metrics::Metrics` | Move `metrics` down to the runtime layer |
| `server`, `acp` → `cli` | `sandbox_runtime::canonical_directory`, `repl_commands::skill_report` | Move both functions to the runtime layer |
| `tui` → `cli` | `repl`, `repl_commands`, `login` | Keep `tui` in the same crate as `cli` |

Inside the runtime, many modules still depend on each other in both
directions: `tool` ↔ `app`, `tool` ↔ `subagent`, `tool` ↔ `mcp`,
`session` ↔ `failover`, and `approval` → `reflection`/`subagent`/`workflow`.
These stay in one crate. Breaking them is the larger refactor and is not
proposed here.

## Proposed crates

| Crate | Contents | Production / test lines |
|---|---|---|
| `otto-base` | the leaves listed above | 12.2k / 6.8k |
| `otto-runtime` | `config`, `session`, `failover`, `sandbox`, `mcp`, `tool`, `skill`, `approval`, `subagent`, `workflow`, `schedule`, `reflection`, `app`, `metrics`, plus the moved `cli` modules | about 53k / 39k |
| `otto-client` | `client` | 1.5k / 0 |
| `otto-server` | `server` | 6.6k / 7.6k |
| `otto` (binary, plus a lib for the integration tests) | `cli`, `tui`, `acp` | about 26k / 18k |

Dependencies point one way: `otto-base` ← `otto-runtime` ← {`otto-client`,
`otto-server`} ← `otto`. Test helpers such as `cli::testutil` that other
layers use move to a `testutil` module behind a `test-support` feature of the
crate that owns them. The feature is enabled only through `dev-dependencies`.

## Expected gain, stated honestly

The critical path becomes `otto-base` → `otto-runtime` → `otto`. That is
91k of the 92k production lines, still in sequence. Only `otto-server` and
`otto-client` (8k) move off the path. So **the lib build in CI gets little
faster**. The gains come from elsewhere:

1. **Test targets run in parallel.** Today the 71k test lines compile as one
   target. After the split, each crate's test target compiles as soon as
   that crate's metadata is ready, alongside the next layer. Most test code
   (39k lines) is in `otto-runtime`, which builds early.
2. **Clippy follows the same pattern.** The check builds of the layers run
   their test targets in parallel.
3. **Local rebuilds.** A change to `cli`, `tui`, `server` or `acp` no longer
   re-analyses the 53k runtime lines. This is the biggest win, but it only
   helps local development: CI starts from a clean workspace build on every
   run, because the cache stores only third-party crates.

Without building it I can only estimate: 1 to 2 minutes off the macOS job's
6m45s of workspace compilation, and less on Linux. CI runners have only 3
(macOS) or 4 (Linux) cores, which caps how much the parallelism can help.

## Cheaper alternative to weigh first

**Run clippy as a separate job.** Clippy and the test build run one after
the other in the same macOS job. As a separate parallel job, clippy would
take its 2m00s off the macOS critical path at no code risk. The cost is one
more macOS runner per run. It can be done alone or together with the split.

## Plan if approved

Each phase is its own pull request, keeps `make check` and `make check-linux`
green, and changes no behavior:

1. **Move misplaced code within the crate.** Add an architecture test that
   fails when a runtime module imports `cli`, `tui`, `acp`, `server` or
   `client`. This phase stands on its own: it gives `cli` one direction of
   dependency whether or not the split goes ahead. As implemented, these
   moved to `crate::runtime`:
   - `cli::runtime_builder`, which became `runtime::builder`, with the
     `chatgpt_client` it took from `cli::login`.
   - `cli::{wiring, info}`, and the modules they need: `prompt`,
     `workspace_context`, `boundary`, `sandbox_runtime` (which holds
     `canonical_directory`) and `advice`.
   - The `[sandbox]` editing half of `cli::sandbox_setup`, which became
     `runtime::sandbox_config`; the interactive `otto sandbox setup` command
     stays in `cli`.

   `server::metrics` became `crate::metrics`. `repl_commands::skill_report`
   stays in `cli`: `acp` and `cli` remain one crate in this plan.
2. **Extract `otto-base`.** Move the leaves into their own crate. Measure
   with `--timings`.
3. **Extract `otto-runtime`, `otto-client` and `otto-server`.** Update the
   code organization map, the task map in `AGENTS.md`, and the development
   guide.
4. **Measure on CI against #584.** Stop after any phase where the measured
   gain is not worth continuing.

## Risks

- Phases 1 and 3 move about 60k lines. Open pull requests that touch moved
  files will conflict, so each phase should merge quickly and on its own.
- `pub(crate)` items used across the new crate boundaries become `pub`. That
  widens their visibility inside the workspace, but this is not a published
  library.
- Process-wide registrations, such as `failover::global`, which `mcp` and
  `sandbox` use, must stay in exactly one crate. Two copies of a static would
  mean two registries.
- The wasm boundary is unaffected: `otto-core` and `otto-web` do not depend
  on `crates/otto`.

## Decision needed

1. Approve phases 1 to 4 as above, or only phase 1 (the in-crate move,
   useful on its own).
2. Whether to do the separate clippy job, and whether before the split or
   instead of it.
