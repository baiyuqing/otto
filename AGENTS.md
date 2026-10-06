# Repository rules

Read this file first, then the `AGENTS.md` files along the directory path you
work in. Child files add scoped rules; repository rules still apply. Use the
[code organization map](docs/code-organization.md) for ownership and entry
points and the [development guide](docs/development.md) for detailed contracts.

## Find the owning layer

| Work | Instructions and entry point |
| --- | --- |
| Shared domain/config/wire logic | [otto-core rules](crates/otto-core/AGENTS.md), `crates/otto-core/src/` |
| Native runtime, tools and frontends | [otto rules](crates/otto/AGENTS.md), `crates/otto/src/cli/` |
| Documentation | [documentation rules](docs/AGENTS.md), `docs/user-manual.md` |
| Browser frontend | [otto-plus](https://github.com/baiyuqing/otto-plus); separate client of Otto HTTP APIs |
| Chat connector | `chat-connector/`; independent Go ACP client; [design](docs/specs/2026-10-02-otto-connect.md) |

Otto supports only `openai-compatible` and `chatgpt`. Do not implement or
advertise other providers or planned roadmap features.

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
  [changelog rules](docs/development.md#changelog).
- Preserve append-only session history and compatibility. Keep dependencies
  explicit and one-way, reuse existing contracts, validate trust boundaries,
  and add abstractions only at real consumer boundaries. Detailed
  [implementation contracts](docs/development.md#agent-implementation-contracts)
  apply to every feature, fix and refactor.
- If work is delegated, the parent owns interfaces, integration and acceptance;
  hand off bounded file ownership and preserve other workers' changes.
- Adding, changing, removing or renaming a model-callable capability updates
  its [capability descriptions](docs/development.md#capability-descriptions)
  in the same PR. Descriptions are short and unambiguous and state the
  capability, inputs and outputs. This rule is mandatory.
- Version numbers change only in release PRs via `make bump VERSION=X.Y.Z`.
- After implementation and required checks, open a PR without asking again.

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

## Verification

Run focused behavioral tests, then `make check-fast`. Feature/fix tests must
fail when the added/fixed behavior breaks. Documentation-only changes need
no new tests. Never run `make check` or `make check-linux` locally; CI runs
both full gates. Use [local verification](docs/development.md#local-verification)
for selection and platform-specific contracts.

Full sandbox validation requires macOS 26+ and standalone Command Line Tools;
Linux checks cannot establish Seatbelt behavior. CI commands/toolchain pins
live in [.github/workflows/checks.yml](.github/workflows/checks.yml).
