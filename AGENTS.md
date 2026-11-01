# Repository rules

Read this file first, then the `AGENTS.md` files along the directory path you
work in. Child files add scoped rules; repository rules still apply. Use the
[code organization map](docs/code-organization.md) for ownership and entry
points and the [development guide](docs/development.md) for detailed contracts.

Keep these files short: each directory's `AGENTS.md` is preferred over its
`CLAUDE.md`, Otto reads them at startup, and one file over 16 KiB or 64 KiB in
total fails startup. Describe rules here; link to the canonical contract or
the owning code instead of restating it.

## Find the owning layer

| Work | Instructions and entry point |
| --- | --- |
| I/O-free contracts, formats and agent loop | [otto-core rules](crates/otto-core/AGENTS.md), `crates/otto-core/src/` |
| Native runtime, tools, TUI and `embed` | [otto rules](crates/otto/AGENTS.md), `crates/otto/src/runtime/startup.rs` |
| HTTP serve frontend | [otto-server rules](crates/otto-server/AGENTS.md), `crates/otto-server/src/lib.rs` (`Server`) |
| ACP stdio and attach frontends | [otto-acp rules](crates/otto-acp/AGENTS.md), `crates/otto-acp/src/lib.rs` |
| `otto` binary, commands, composition root | [otto-cli rules](crates/otto-cli/AGENTS.md), `crates/otto-cli/src/run.rs` (`otto_cli::run`) |
| Documentation | [documentation rules](docs/AGENTS.md), `docs/user-manual.md` |
| Browser frontend | [otto-plus](https://github.com/baiyuqing/otto-plus); separate client of Otto HTTP APIs |
| Chat connector | `chat-connector/`; independent Go ACP client; [guide](docs/manual/servers-and-connectors.md#chat-connector) |

Otto supports only `claude`, `openai-compatible` and `chatgpt`. Do not
implement or advertise other providers or planned roadmap features.

## Workflow

- Every feature/fix uses a development branch and a dedicated
  `.worktree/<name>` under the primary checkout; never implement on `main`.
- Track multi-step work in ignored `.worktree/<name>.plan.md`. Record the
  problem, dated user decisions, completed/open items, limitations and agreed
  follow-up work; update it as decisions and steps finish.
- When a design/spec is needed, obtain explicit approval before production
  code or tests. After approval, complete the agreed scope without repeated
  confirmation; clarify only material scope/contract changes.
- Ship requested features enabled by default. Required user input, such as an
  environment API key, may determine whether a feature can run.
- Update code, schemas, tests and canonical docs together. Notable changes
  update [CHANGELOG.md](CHANGELOG.md) using the
  [changelog rules](docs/development/workflow.md#changelog).
- Preserve append-only session history and compatibility. Keep dependencies
  explicit and one-way, reuse existing contracts, validate trust boundaries,
  and add abstractions only at real consumer boundaries. Detailed
  [implementation contracts](docs/development/contracts.md#agent-implementation-contracts)
  apply to every feature, fix and refactor.
- If work is delegated, the parent owns interfaces, integration and acceptance;
  hand off bounded file ownership and preserve other workers' changes.
- Adding, changing, removing or renaming a model-callable capability updates
  its [capability descriptions](docs/development/contracts.md#capability-descriptions)
  in the same PR. Descriptions are short and unambiguous and state the
  capability, inputs and outputs. This rule is mandatory.
- Version numbers change only in release PRs via `make bump VERSION=X.Y.Z`.
  Feature and fix PRs never edit version files, whatever a remembered
  preference about version increments says.
- Use small, focused commits with imperative subjects such as
  `feat: add ...` or `docs: document ...`.
- After implementation, `cargo fmt --all` and the focused tests, open a PR
  without asking again. Opening the PR ends the task.

## Code size and naming

- A code file (`.rs`, `.go`, `.js`, `.mjs`, `.py`, `.sh`) has at most 800
  lines, tests included, and a line at most 200 characters. Before a change
  would cross the limit, split the file into child modules named by domain.
- Name variables and functions for `grep`: state what the value holds
  (`agent_file_bytes`, not `data`, `tmp2` or `a1`).
- Name files and modules after their domain (`memory_types.rs`), never a
  generic container: `utils`, `util`, `helpers`, `helper`, `common`, `misc`,
  `types`, `config`.
- A Rust file under `crates/*/src` has a directory or file named `tests` or
  `<domain>_tests` in its path exactly when it is compiled only under
  `#[cfg(test)]`.
- The pre-commit hook and `make code-limits` enforce the size and file-name
  rules; see [code limits](docs/development/workflow.md#code-limits).

## Safety

- API keys and chat secrets come only from environment variables; ChatGPT
  credentials come from `otto login`. Never store secrets in config,
  fixtures, logs, docs, tests or skill files.
- File tools reject workspace/skill-directory escapes after canonical-path
  and symlink validation. Use shared pinned filesystem operations.
- Bash goes through native `sandbox`; only explicit `off` is unsandboxed.
  It always starts in the selected workspace.
- Default tests stay offline: no live providers, real credentials or external
  services. PTY acceptance uses controlled test terminals.
- Install the repository's pre-commit hook once per clone with
  `make setup-git-hooks` (it needs `gitleaks` and `node` on `PATH`). The hook
  is shared with this clone's worktrees and blocks a commit whose staged
  changes hold a secret finding or break the code limits, or that it cannot
  scan.

## Verification

The repository has no CI; no build, lint or test gate runs on a PR. Before
committing a Rust change, run `cargo fmt --all` and the tests that cover the
changed code; run the full `make` gates only when asked. The PR body lists
each test command, its result and the cause of any failure. Feature/fix tests
must fail when the added/fixed behavior breaks; when that is not evident from
the test, name in the PR the assertion that fails without the change instead
of reverting it locally. Documentation-only changes need no new tests. See
[local verification](docs/development/workflow.md#local-verification) for the
`make` gates.

Full sandbox validation requires macOS 26+ and standalone Command Line Tools;
Linux checks cannot establish Seatbelt behavior. The Rust toolchain pin lives
in [rust-toolchain.toml](rust-toolchain.toml).
