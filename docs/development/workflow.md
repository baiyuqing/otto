# Development workflow

Previous: [Architecture](architecture.md) · [Documentation index](../README.md) · Next: [Testing](testing.md)

## Development isolation and ownership

Choose the cheapest adequate model for routine work and escalate only when the
task requires it. Delegated work must have bounded file ownership; hand off
overlapping files before another worker edits them and preserve other workers'
changes.

Use a dedicated Git worktree and development branch for every feature or bug
fix. Never implement directly on `main`. If implementation changes are
accidentally made on `main`, restore them before continuing in the worktree.

Put each worktree at `.worktree/<name>` under the primary checkout, not as a
sibling directory of the repository:

```bash
git worktree add .worktree/<name> -b feat/<name>
```

`.worktree/` is gitignored so checkouts are not committed. Cursor also
imports `.gitignore` into the agent workspace; `.cursorignore` un-ignores
`.worktree/` so file tools do not treat a worktree as outside the project.
After the pull request merges, remove that worktree and its local branch
(`git worktree remove .worktree/<name>`, then `git branch -d`).

When work requires a design or spec, finish the discussion and get explicit
approval before writing production code or tests. Once approved, carry the
agreed work through implementation and verification without repeated
confirmation; seek clarification only for material scope or contract changes.

## Rust workflow

Full host validation uses macOS 26+ with standalone Command Line Tools selected.
Linux builds and runs Otto without a sandbox, so its gate is `make check-linux`,
which CI runs on `ubuntu-24.04`: the same format, lint, test, wasm-check and
PTY targets, minus the Seatbelt conformance suite, which is compiled only for
macOS, and minus the release build, the Node wasm tests, and the web UI, which
are platform independent and stay on the macOS job. Keep platform-specific
code behind `cfg` rather than runtime checks, so a target that cannot use it
does not compile it.
On macOS, CI runs `make check` as two parallel jobs: `macOS lint` runs
`rust-fmt` and `rust-lint`, and `macOS acceptance` runs `make check-tests`,
which is everything else in `check`. Clippy compiles the workspace a second
time, so running it beside the tests takes it off the critical path; the lint
job keeps its own dependency cache, since the tests' cache holds no
check-mode metadata.
A documentation-only change skips every gate. The workflow's `changes` job
classifies the diff first: when every changed path is Markdown outside
`crates/` and `testdata/`, under `docs/`, or `LICENSE`, the gates start and
skip every step, so they still report a status without installing a toolchain.
Anything else — including a workflow edit, `testdata/server/openapi.yaml`, or a
Markdown fixture the skill and agent loaders discover by name — runs the full
gate. An unusual range, such as a new branch or a force push that dropped the
old tip, also runs it.

### CI build cache

Most of a CI run is compilation, so the workflow keeps it down in three ways:

- It sets `CARGO_INCREMENTAL=0` and `line-tables-only` debuginfo for the dev
  and test profiles. A CI build never reuses incremental state, and line
  tables are enough for a failing test's backtrace. Local builds keep Cargo's
  defaults.
- The cargo cache holds the registry and the third-party part of `target/`,
  keyed by the root `Cargo.toml` (which holds the profiles), `Cargo.lock` and
  `rust-toolchain.toml`. A run saves it only when no
  cache matched the key exactly, after `node scripts/ci-cache.mjs prune`
  removes the workspace crates' own output, which changes with every commit
  and is most of `target/`. A failed gate still saves once
  `cargo test --workspace --no-run` finishes the build, since the key is
  saved only once and a failing test would otherwise leave every later run
  building cold. A push to `main` restores only an exact match, so
  the cache it saves never accumulates artifacts of older lock files; a pull
  request may fall back to `main`'s previous cache. Bump the `v2-` key prefix
  when a workflow change makes the cached artifacts unusable, such as the
  `CARGO_PROFILE_*` variables.
- `wasm-pack` and `wasm-bindgen-cli` are cached by version, saved right
  after they are installed. wasm-pack uses the
  `wasm-bindgen` on `PATH` when it matches the version `Cargo.lock` pins
  (`node scripts/ci-cache.mjs lock-version wasm-bindgen`); without it,
  wasm-pack compiles its own copy on every run.

Each gate builds `crates/otto` once per feature set, so its targets must not
ask Cargo for a different one; see `connect-check` below. The root
`Cargo.toml` builds the Turso crates at `opt-level = 2` in the dev profile:
the stores' tests spend most of their time in Turso, and an optimized Turso
runs them about three times faster at a compile cost paid once per Turso
version.

The copied Apple broker fixtures are ad-hoc-signed arm64e executables, which
require the third-party arm64e support introduced in macOS 26. CI selects
`/Library/Developer/CommandLineTools` so Git and Clang use the existing reviewed
developer read root. See [the workflow](../../.github/workflows/checks.yml) for pins.

`rust-toolchain.toml` pins the exact channel (1.98.0) and the
`wasm32-unknown-unknown` target; `rustup toolchain install` in a repository
checkout installs that toolchain without asking. Workspace dependencies in
`Cargo.toml` are exact-pinned (`=x.y.z`), so `cargo update` never silently
changes a dependency version.

The canonical Make targets are:

```bash
make check-fast     # manual use only: the checks for paths changed since origin/main
make check          # full macOS gate: rust-fmt, rust-lint, all tests (PTY tests included), core wasm tests (no release build)
make check-tests    # `check` without rust-fmt and rust-lint (CI's macOS acceptance job)
make build          # the release build: cargo build --release, then copy ./otto
make install        # build and install otto, otto-connect and bundled skills
make rust-fmt       # cargo fmt --all -- --check
make rust-lint      # cargo clippy --workspace --all-targets -- -D warnings
make rust-test      # cargo test --workspace (offline)
make rust-wasm-check # cargo check -p otto-core for wasm32-unknown-unknown
make rust-wasm-test  # wasm-pack test --node for otto-core
make test-tui       # only the PTY tests: cargo test -p otto --test tui_pty --test tui_attach_pty
make check-linux    # the Linux gate: rust-fmt, rust-lint, rust-test, connect-check, rust-wasm-check, scripts-test, license-check
make license-check  # fail if a shipped Rust crate uses a license outside the allowlist
make connect-check  # Go connector: gofmt -l, go vet, go test -race (builds target/debug/otto for its end-to-end test)
make connect-build  # build the Go connector to target/otto-connect
```

`check-fast` is described under [Local verification](workflow.md#local-verification).
`check` runs `rust-fmt` and `rust-lint`, then runs the full workspace test suite (which
includes the `otto-core` tests and the PTY tests), the Go connector
checks, and the core wasm tests under Node. It does not run `cargo build
--release` or `rust-wasm-check`; `make build` is the release build, and
`make check-linux` runs `rust-wasm-check`.

`connect-check` needs the Go version named in `chat-connector/go.mod`; CI installs it
with `actions/setup-go`. It gets `target/debug/otto` from
`cargo test --workspace --no-run`, which resolves the same features as
`rust-test` and so reuses its build; `cargo build -p otto` would leave the
dev-dependency features off and recompile most of the dependency tree. Its end-to-end tests drive the real
`otto acp`, and `otto serve` with `otto acp --attach`, from `target/debug/otto`
against a loopback fake provider, so they are offline like the Rust tests.

`rust-wasm-test` needs `wasm-pack` (pinned to
0.15.0 in CI via `cargo install wasm-pack --version 0.15.0 --locked`).
The PTY tests (`tui_pty`, `tui_attach_pty`) open a pseudo-terminal, which is
unavailable in some sandboxed shells; `cargo test --workspace` runs them, and
`make test-tui` runs only them. Neither gate runs `test-tui` separately:
`cargo test -p otto` resolves features differently from `--workspace` and
would compile `crates/otto` again just to repeat the same tests. Keep the default test suite (`cargo test --workspace` without extra
flags) offline: it must not need network access, provider credentials, or a
real interactive terminal.

The Node helpers under `scripts/` are opt-in, but their tests are not: `make
scripts-test` runs every `scripts/*.test.mjs`, and `check`, `check-tests` and `check-linux` include it. It
needs no build and no network.
`scripts/otto.sh` is not a Node helper but the shell launcher documented in the
user manual; `scripts/otto.test.mjs` exercises it offline against a fake binary
and a temporary `otto.env`.
`scripts/otto-tcp.sh` is the supervisor for the ACP TCP deployment (`otto serve
--acp-listen` plus an `otto-connect` configured with `[agent] address`);
`scripts/otto-tcp.test.mjs` starts it offline against a fake `otto` and a fake
`otto-connect` and covers the argument translation, the `connect.toml` checks,
restart, stop and status.
`scripts/pi-session-interop.mjs` covers the optional Pi interoperability probe
described in the user manual. `scripts/skill-exec-measure.mjs` reports what the
[sub-agent execution design](../specs/2026-09-22-skill-subagent-execution.md) says
must be measured before that feature is trusted: context peak per window, total
tokens, and how often the model honours the `exec="agent"` marking. It reads a
session transcript and `~/.otto/usage.db`, never writes, and needs a real
session to have happened first, so it stays out of the gates even though its
tests are in them. `scripts/trace-viewer.html` is retained from the
Go implementation: the Rust build has no `OTTO_TRACE` writer (the name is only
reserved in the resolution environment), so nothing in this repository produces
the JSONL files the viewer reads. Do not document provider tracing as a working
feature until a writer exists.

### Dependency licenses

`make license-check` runs `scripts/license-check.mjs`, and `make check-linux`
runs it. It reads the `license` field of every Rust crate in the non-dev
dependency graph (`cargo metadata --locked`, all targets), and fails when a license expression is not
satisfied by the allowlist `ALLOWED`: MIT, Apache-2.0, BSD-2-Clause,
BSD-3-Clause, ISC, Zlib, Unicode-3.0 and Unlicense. An `OR` expression passes
when one alternative is allowed; `AND` requires all. The check needs network
access in CI only if `cargo metadata` has to fetch crates. `cargo metadata`
resolves features for all targets together with dev-dependencies, so the
checked set also contains optional crates that a release build does not
compile (36 of 479 on 2026-10-05); such a crate can fail the check without
shipping.

Dev-only Rust crates are not checked because they do
not ship. The Go module under `chat-connector/` is not checked because
`go.mod` carries no license metadata.

When the check fails, read the package's license. If it is acceptable, add its
identifier to `ALLOWED`. For a package without a license field, read its
license file and add a `name@version` entry to `CLARIFIED`; the version is part
of the key, so a new version is reviewed again.

Otto's binaries are not distributed, so no third-party notice file ships. If
binaries are distributed, the MIT, BSD and Apache-2.0 terms require one.

### Local verification

CI runs `make check` and `make check-linux` on every PR; do not build or test
locally. Before committing a Rust change, run only `cargo fmt --all`. Opening
the PR ends the task; do not poll CI. When asked to fix a failed run, read
only the failed jobs' log with `gh run view <run-id> --log-failed`, fix the
cause, and push; do not rerun the gate locally to reproduce it.

`make check-fast` remains for manual use. It runs `scripts/check-changed.mjs`,
whose `plan` function maps the paths changed since
`git merge-base HEAD origin/main` to the commands that check them.

### ACP integration-test navigation

`crates/otto/tests/acp_stdio.rs` and `acp_attach.rs` retain the two ACP test
binaries and their process/JSON-RPC harnesses. Their scenario modules keep the
suite navigable:

- `acp_stdio/{protocol,memory,approval,commands,tasks}.rs` covers the local
  stdio server's wire contract, memory review, permission dialogue, commands,
  cancellation, task queueing, signals, and wake turns.
- `acp_attach/{relay,approval,commands}.rs` covers the relay through
  `otto serve`: session parity/queueing/failure, relay permissions, and
  commands, reflection cancellation, and wake turns.

Shared ACP protocol, session, approval, or serve changes affect both
targets; add coverage to the scenario module that owns the behavior.

## Web UI workflow

The browser frontend and WASM bindings live in
[otto-plus](https://github.com/baiyuqing/otto-plus). Build and test them there.
Otto builds independently, serves authenticated HTTP APIs, and keeps its core
WASM conformance checks. Coordinate API/schema changes with the frontend's
pinned core revision and wire declarations; use its same-origin proxy rather
than adding CORS or static assets to Otto.

## Secret scanning

Install Gitleaks (`brew install gitleaks` on macOS), then run
`make setup-git-hooks` once per clone. The native pre-commit hook scans staged
changes and blocks commits on findings or scanner errors, including a missing
Gitleaks binary. The hook is shared with this clone's worktrees; installation
refuses to overwrite a different hook. Other clones need their own setup.

The independent **Secret scan** CI job scans full Git history on every PR and
push to `main`, including documentation-only changes. It uses Gitleaks 8.30.1
with a pinned download checksum and redacted output. Configure this job as a
required status check for `main`. GitHub Secret Scanning and Push Protection
provide complementary server-side detection for supported secret types.

For a manual scan, run `gitleaks git -v --redact --ignore-gitleaks-allow` from
the repository root. Inline `gitleaks:allow` comments do not bypass the hook
or CI. The root `.gitleaksignore` lists reviewed historical false positives
by exact fingerprint, with a reason for each group. All files and new commits
remain scanned. Before adding an entry, inspect the finding in its original
commit and confirm its provenance; do not ignore whole test directories,
rules, or commits. Changes to ignore lists or scanner configuration need the
same careful review as security-sensitive code.

Hooks can be bypassed and CI runs after upload; neither guarantees that secrets
never reach the remote. Keep real credentials outside tracked files. If a real
secret is uploaded, revoke or rotate it immediately; deleting it in a later
commit leaves it in history.

## Changelog

[CHANGELOG.md](../../CHANGELOG.md) is the curated record of notable user and
contributor changes. Update it in the same PR as features, fixes, compatibility
changes, security changes, performance improvements, or build/CI changes.
Documentation changes that affect how people use or develop Otto also belong
there; typo-only edits, internal refactors with no observable effect, and
changelog maintenance itself need no entry.

- Add pending entries under `Unreleased`. Use these categories in order:
  `Usage and compatibility`, `Features`, `Fixes`, `Security`, `Performance`,
  `Build and CI`, and `Documentation`. Omit empty categories.
- Describe the resulting behavior and its practical impact in concise English,
  with a PR link (or commit link when there is no PR). Group related changes;
  do not copy the commit log. After `gh pr create` prints the PR number,
  add the link, commit and push in one shell command. The
  complete historical index is a fixed initial backfill, not a per-PR ledger.
- Put breaking changes, changed defaults, migration steps, and required user
  action in `Usage and compatibility`. Link the manual for detailed setup.
  Use other categories for distinct aspects of the same change without
  repeating the same explanation. Do not claim unmeasured speedups.
- In a release PR, move the pending entries into `## X.Y.Z - YYYY-MM-DD`
  using the intended release date and leave `## Unreleased` above it.
  Keep releases newest first and preserve prior release entries and the fixed
  historical index. `make bump`
  updates version files only; this changelog step is manual.

## Releases

The version is `[workspace.package] version` in `Cargo.toml`, shared by `otto` and `otto-core`. The `otto` and `otto-core` entries of `Cargo.lock` must carry the same value; a test fails `make check` when they differ.

Versioning is patch-only in the 0.x range: a release increments the last number (0.4.0 to 0.4.1), and neither a new feature nor an incompatible change raises the minor version. Moving to 1.0.0 is a maintainer decision. Version numbers change only in a release PR.

1. On a branch, run `make bump VERSION=X.Y.Z`. `scripts/release.mjs` requires X.Y.Z to be a plain version greater than the current one, rewrites `Cargo.toml` and `Cargo.lock` by exact text replacement (no other line changes, no network), and exits with an error if a location is missing or matches more than once.
2. Archive the `Unreleased` changelog entries as described above. Commit, open a PR titled `Release vX.Y.Z`, and merge it.
3. On an up-to-date `main`, run `make release-tag`. It checks that HEAD equals `origin/main`, the working tree is clean and the tag does not exist locally or on origin, then creates the annotated tag `vX.Y.Z`, pushes it and runs `gh release create` (needs `gh`).

Release notes are generated by GitHub from the PR titles merged since the previous tag. The curated, categorized history lives in [CHANGELOG.md](../../CHANGELOG.md); review it alongside the generated notes.

### Build metadata

`crates/otto/build.rs` embeds the output of `git rev-parse --short=12 HEAD`, plus `.dirty` when tracked files were modified when the build script last ran. It reruns when HEAD, the index, the current ref, `packed-refs` or anything under `crates/` changes. When git is unavailable it uses the `OTTO_GIT_HASH` environment variable, then `unknown`. The result is `otto::VERSION` (`0.1.0+a1b2c3d4e5f6`), printed by `otto --version` and reported in the ACP agent info, the MCP client info and the `otto_help` output.

---

Previous: [Architecture](architecture.md) · [Documentation index](../README.md) · Next: [Testing](testing.md)
