# Repository rules

Read this file and the `AGENTS.md` files along the path you change; scoped rules
add to these. Use the [ownership map](docs/code-organization.md) and
[development guide](docs/development.md) for details instead of copying their
inventories here. Otto prefers `AGENTS.md` over `CLAUDE.md` and rejects an
instruction file over 16 KiB or combined instructions over 64 KiB.

## Find the owner

| Change | Start here |
| --- | --- |
| I/O-free contracts, formats, agent loop | [otto-core](crates/otto-core/AGENTS.md) |
| Native services, tools, persistence, sandbox, TUI | [otto](crates/otto/AGENTS.md), `src/runtime/startup.rs` |
| HTTP routes and `otto serve` | [otto-server](crates/otto-server/AGENTS.md), `src/lib.rs` |
| ACP stdio and attach | [otto-acp](crates/otto-acp/AGENTS.md), `src/lib.rs` |
| Binary, commands, composition root | [otto-cli](crates/otto-cli/AGENTS.md), `src/run.rs` |
| User/contributor docs | [docs](docs/AGENTS.md) |

Dependencies point `otto-core` <- `otto` <- `otto-server` <- `otto-acp` <-
`otto-cli`. The browser client is in the separate `otto-plus` repository;
`chat-connector/` is a separate Go ACP client, not a Cargo crate.
Only `claude`, `openai-compatible` and `chatgpt` providers are supported.

## Work and release boundaries

- For features/fixes, branch in `.worktree/<name>` under the primary checkout;
  never implement on `main`. Track multi-step work in ignored
  `.worktree/<name>.plan.md` with decisions, progress and limitations. Obtain
  explicit design approval when a design/spec is needed before code or tests.
- Keep session history append-only and compatible; use shared pinned filesystem
  operations for workspace/skill paths. Native Bash runs through `sandbox`;
  only explicit `off` is unsandboxed. See the
  [implementation contracts](docs/development/contracts.md#agent-implementation-contracts).
- Changes to model-callable capabilities must update their short input/output
  [descriptions](docs/development/contracts.md#capability-descriptions) in the
  same PR. Update affected code, schemas, tests and canonical docs together;
  record notable impacts under `Unreleased` per the
  [changelog rules](docs/development/workflow.md#changelog).
- Version files change only in release PRs via `make bump VERSION=X.Y.Z`, never
  in a feature/fix PR. Ship requested features enabled by default unless
  required user input (such as an environment key) is absent.
- Keep API keys and chat secrets in environment variables, never tracked files
  or logs; ChatGPT credentials come from `otto login`.

## Local verification

- There is no CI gate. Before committing Rust changes run `cargo fmt --all`
  and focused tests, e.g. `cargo test -p otto --lib approval` or
  `cargo test -p otto-cli --test tui_pty`. A whole crate runs with
  `cargo test -p otto-server`; the native workspace suite is
  `cargo test --workspace`. Tests are offline by default; no live provider or
  real credentials. PTY tests use controlled test terminals.
- `cargo build -p otto-cli` builds the debug executable; `make build` builds
  release and copies it to `./otto`. `make run` launches that binary using
  `otto.env`, so build first. `make install` also builds the Go connector and
  installs bundled skills.
- `make check-fast` selects checks for paths changed since `origin/main` (or
  `main`), including untracked files; `make test-tui` runs both PTY targets.
  `make connect-check` builds the test binary and runs Go vet/race tests.
  Run full `make check` or `make check-linux` only when asked. The full gate
  does not build a release binary or browser frontend. See [local verification](docs/development/workflow.md#local-verification).
- Documentation-only edits need no new tests: check links, paths, examples and
  Markdown. Include commands, results and failures in the PR body; after
  verification, open the PR without another blanket confirmation.
- Install the shared pre-commit hook once per clone with `make setup-git-hooks`
  (requires Gitleaks and Node). It scans staged secrets and enforces code
  limits. Code files (`.rs`, `.go`, `.js`, `.mjs`, `.py`, `.sh`) are limited to
  800 lines and 200 characters per line. `make code-limits` also rejects generic
  names (`utils`, `util`, `helpers`, `helper`, `common`, `misc`, `types`, `config`)
  and requires Rust test-only files under `crates/*/src` to have `tests` or
  `<domain>_tests` in their path (and vice versa).
- Full sandbox acceptance requires macOS 26+ with standalone Command Line
  Tools; Linux tests do not validate Seatbelt. The pinned Rust toolchain is in
  `rust-toolchain.toml`.
