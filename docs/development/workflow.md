# Development workflow

[Development guide](../development.md) · [Testing](testing.md) · [Contracts](contracts.md)

## Development isolation and ownership

Use a development branch and dedicated `.worktree/<name>` under the primary
checkout for every feature/fix; never implement on `main`:

```bash
git worktree add .worktree/<name> -b feat/<name>
```

Track multi-step work in ignored `.worktree/<name>.plan.md`: problem, dated
user decisions, completed/open work, limitations and agreed follow-ups.
Delegation needs bounded file ownership; preserve other workers' changes.
When a design/spec is needed, obtain explicit approval before production code
or tests, then complete the agreed scope without repeated confirmation.
After merge, remove the worktree and its local branch.

## Rust workflow

Use the pinned `rust-toolchain.toml` toolchain (currently 1.99.0) and its
`wasm32-unknown-unknown` target. Workspace dependencies are exact-pinned in
`Cargo.toml`. Keep platform-specific code behind `cfg`.

Full sandbox acceptance requires macOS 26+ and standalone Command Line Tools
selected at `/Library/Developer/CommandLineTools`; the arm64e broker fixtures
need macOS 26 support. Linux builds/runs without a sandbox and cannot establish
Seatbelt behavior.

Canonical targets (see root `Makefile`):

| Command | Purpose |
| --- | --- |
| `make build` | Release binary copied to `./otto` |
| `make install` | Build/install Otto, otto-connect and bundled skills |
| `make rust-fmt` | `cargo fmt --all -- --check` |
| `make rust-lint` | Workspace/all-targets Clippy with warnings denied |
| `make rust-test` | Workspace native tests, including PTY tests |
| `make test-tui` | Only `tui_pty` and `tui_attach_pty` |
| `make rust-wasm-check` | Check portable core for wasm32 |
| `make rust-wasm-test` | Core WASM tests under Node (`wasm-pack`) |
| `make connect-check` | Build test runtime; Go formatting, vet and race tests |
| `make scripts-test` | Offline Node tests beside scripts |
| `make license-check` | Rust non-dev dependency license policy |
| `make code-limits` | File length, line length and file-name limits |
| `make check` | macOS: formatting, lint and `check-tests` |
| `make check-tests` | Native, connector, WASM and script tests; diff whitespace check |
| `make check-linux` | Formatting, lint, native/connector tests, WASM check, script/license checks |
| `make check-fast` | Manual path-selected checks since merge-base with `origin/main` |

The gates do not build a release binary or browser frontend. Keep Cargo feature
sets aligned across targets: `connect-check` uses `cargo test --workspace
--no-run` to reuse the native test build, not a separate `cargo build` feature
set. Default tests remain offline; live provider probes are opt-in.

### Dependency licenses

`scripts/license-check.mjs` reads the locked non-dev Rust dependency graph,
including optional/target-specific crates resolved by Cargo. `OR` needs one
allowed alternative; `AND` needs all. `ALLOWED` is the canonical allowlist.
Review a package's license before extending it. Missing metadata requires a
reviewed `name@version` entry in `CLARIFIED`, so upgrades get reviewed again.
Dev-only Rust and the Go module are outside this metadata check. Cargo may
need network access to fetch missing crates. Binary distribution also requires
review of applicable third-party notice obligations.

### Local verification

The repository has no CI; no gate runs automatically on a PR. Run full
`make check`/`make check-linux` only when asked. Before committing Rust changes,
run `cargo fmt --all` and tests covering the changed behavior, for example:

```bash
cargo test -p otto --test storage_boundary
cargo test -p otto --lib approval
```

Docs-only changes need no new tests: check links, paths, examples and Markdown.
Report each verification command, result and cause of failure in the PR body;
do not hide failing gates or weaken checks. Use focused imperative commits.
After implementation and verification, open the PR without another blanket
confirmation; opening it ends the task.

### Code limits

`scripts/code-limits.mjs` enforces the size and file-name rules of
[AGENTS.md](../../AGENTS.md#code-size-and-naming) on code files (`.rs`, `.go`,
`.js`, `.mjs`, `.py`, `.sh`): at most 800 lines per file, at most 200 Unicode
code points per line, and no file or Rust module named `utils`, `util`,
`helpers`, `helper`, `common`, `misc`, `types` or `config`. A Rust `mod.rs`
counts under its directory name and a Go `_test` suffix is ignored.
`GENERIC_NAME_EXCEPTIONS` keeps the two configuration modules,
`otto_core::config` and `otto::config`. No baseline exempts a file from the
size limits.

The pre-commit hook checks staged content (`--staged`). `make code-limits`,
`make check`, `make check-linux` and `make check-fast` check every tracked or
untracked, not ignored, code file in the worktree.

### ACP integration-test navigation

`crates/otto/tests/acp_stdio.rs` and `acp_attach.rs` own the process/JSON-RPC
harnesses. Scenario modules under their matching directories cover protocol,
memory, approvals, commands, tasks and relay behavior. Add cases to the owning
scenario module rather than duplicating harnesses.

## Web UI workflow

The browser frontend and WASM bindings are in
[otto-plus](https://github.com/baiyuqing/otto-plus), not this checkout. Native
build/install and verification do not require a web bundle. Keep shared API
contracts in Otto and client implementation/tests in the frontend repository.

## Secret scanning

Install Gitleaks (`brew install gitleaks` on macOS), then run
`make setup-git-hooks` once per clone. The shared worktree pre-commit hook scans
staged changes with Gitleaks and then applies the [code limits](#code-limits)
to them; it fails on findings, violations or scanner errors, and refuses to
overwrite a different hook. A hook installed before the code limits differs
from `scripts/pre-commit`: delete `$(git rev-parse --git-path hooks)/pre-commit`
and run `make setup-git-hooks` again. There is no CI history scan. For a manual scan:

```bash
gitleaks git -v --redact --ignore-gitleaks-allow
```

Inline allow comments do not bypass scanning. `.gitleaksignore` contains only
reviewed historical false-positive fingerprints with reasons; never exclude
whole directories, rules or commits. Review scanner/ignore changes as security
changes. Hooks can be bypassed: keep real secrets out of tracked files and
revoke/rotate any exposed credential immediately; later deletion is insufficient.

## Changelog

[CHANGELOG.md](../../CHANGELOG.md) summarizes notable resulting user/contributor
impacts, not commits, implementation stages or a complete historical archive.
Git and release tags are the archive. No parallel historical index is required.

- Add concise pending entries under `Unreleased` for observable features,
  fixes, security, compatibility, performance or development/setup changes.
  Internal refactors, typo fixes and changelog maintenance need no entry.
- Group related changes into one resulting outcome; remove superseded claims
  instead of accumulating intermediate behavior. Use categories only when useful;
  omit empty categories and avoid repeating one impact under several headings.
- Put changed defaults, breaking changes, required user action and migration or
  downgrade warnings first. Keep actionable compatibility guidance while it
  still matters; link current manuals instead of copying configuration/protocols.
- Link a PR (or commit) where useful. Do not copy commit lists, add a per-PR
  ledger or claim unmeasured speedups.
- Keep recent release summaries, not indefinitely expanding release detail.
  Older entries may be condensed or removed when superseded; retain only
  still-relevant compatibility guidance. No requirement preserves every prior
  entry or historical index. Editorial pruning itself needs no changelog entry.
- In a release PR, move pending outcomes into `## X.Y.Z - YYYY-MM-DD`, newest
  first, leaving `## Unreleased` above. Use the intended release date.
  `make bump` changes version files only; this editorial step is manual.

## Releases

`[workspace.package] version` in `Cargo.toml` is shared by `otto` and
`otto-core`; their `Cargo.lock` entries must match (covered by tests).
Version changes belong **only in release PRs** via `make bump VERSION=X.Y.Z`,
never feature/fix PRs. Versioning is patch-only in 0.x, even for incompatible
changes; 1.0.0 is a maintainer decision.

1. On a branch, run `make bump VERSION=X.Y.Z`. `scripts/release.mjs` requires
   a plain greater version, rewrites exact manifest/lockfile locations without
   network access, and rejects missing or ambiguous matches.
2. Summarize `Unreleased` under the release heading, commit, open a PR titled
   `Release vX.Y.Z`, and merge.
3. On up-to-date `main`, run `make release-tag`. It requires HEAD equal to
   `origin/main`, a clean tree and an absent local/remote tag; it creates and
   pushes annotated `vX.Y.Z`, then runs `gh release create` (requires `gh`).

GitHub-generated release notes summarize merged PRs; review them alongside the
curated changelog, especially migration instructions.

### Build metadata

`crates/otto/build.rs` embeds the 12-character commit plus `.dirty` for tracked
changes when the build script runs. Without Git it falls back to `OTTO_GIT_HASH`,
then `unknown`. `otto::VERSION` is shown by `--version`, ACP agent info, MCP
client info and `otto_help`. Build metadata is not a release version bump.
