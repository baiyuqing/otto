# Sandbox: protected git metadata and denial reports

Status: approved 2026-10-01 (step 2 of the sandbox usability plan). Step 1
is
[sandbox excluded commands](2026-10-01-sandbox-excluded-commands.md).

## A sandboxed command can write git metadata that git later runs unconfined

Facts from the code at `b14c8e8` (unchanged at `9f6e2c1`):

- The Seatbelt profile has one write rule, `(allow file-write* (subpath …))`,
  for the workspace and the private `HOME`, `TMPDIR` and cache
  (`render_write_rules`, `crates/otto/src/sandbox/seatbelt/profile.rs`). There
  is no deny rule inside the workspace.
- So any sandboxed command can write the workspace's `.git/config` and
  `.git/hooks/`. Neither appears in `git status` or `git diff`.
- Git runs programs named in its configuration (`core.hooksPath`,
  `core.fsmonitor`, `core.sshCommand`, `core.pager`, `alias.<name> = !…`,
  `diff.external`, filter drivers) and in hooks. A file
  `$GIT_DIR/commondir` makes git read the configuration and hooks of another
  directory. A git process started outside the sandbox — the user's terminal
  or editor, an `/approve` grant, or an excluded `git` command — runs these
  programs unconfined.
- Otto's own git calls (the TUI status line in `cli/workspace_context.rs`, the
  server diff in `server/diff.rs`) run inside the sandbox with
  `-c core.fsmonitor=false`.
- `git` is not in `COMMAND_RUNNERS`, so `/approve <id> always` on a git
  command adds `git *`. An excluded `git` runs
  `git -c alias.x='!<command>' x` unconfined.

Claude Code's sandbox (`@anthropic-ai/sandbox-runtime` 0.0.78, macOS) denies
writes to `.git/hooks/**` always and to `.git/config` unless `allowGitConfig`
is set, in the working directory and at any depth below it. It also denies
creating and unlinking each ancestor of those paths, so `.git` cannot be
renamed. It does not protect `commondir` or a `.git` file.

## The workspace repository's git metadata is read-only in the sandbox

With `W` the canonical workspace path, the profile denies `file-write*` on:

- `W/.git` (literal): the entry cannot be created, deleted, renamed or
  written. This covers moving `.git` aside, rewriting the `gitdir:` line of a
  linked worktree's `.git` file, and creating a `.git` in a workspace that is
  not a repository root;
- `W/.git/config`, `W/.git/config.worktree` and `W/.git/commondir`
  (literal);
- `W/.git/hooks` (subpath).

The rules are emitted whether or not the paths exist, and there is no
setting to turn them off. They are rendered after the workspace
`(allow file-write* …)` rule: Seatbelt applies the last matching rule, and
the probe showed that the same denies placed before the allow rule have no
effect.

Effect on commands that run in the sandbox:

- Fail: `git init` in `W`; `git config` without `--global`; `git remote add`
  and `set-url`; `git push -u` (the push completes, writing the upstream
  fails); `git branch --set-upstream-to`; creating a branch that tracks a
  remote branch; `git submodule init`; hook installers such as
  `pre-commit install`.
- Not affected: commit, checkout, branch without tracking, fetch, pull,
  merge, rebase, stash, `git worktree add`, `git clone` into a subdirectory,
  and `git config --global`, which writes the private `HOME`.

A failing command can be run once with `sandbox_permissions =
"require_escalated"` and `/approve <id>`. `git` is added to `COMMAND_RUNNERS`,
so `/approve <id> always` refuses git commands. `/sandbox exclude 'git *'` is
still accepted because the user types it; the manual states that it removes
this protection for git commands.

The Seatbelt policy lines in the system prompt gain one sentence: the
workspace's `.git/config`, `.git/hooks` and the `.git` entry are read-only,
and git commands that write them need `require_escalated`.

### Not covered

- Repositories below `W`: nested clones, submodules (`W/sub/.git` files and
  `W/.git/modules/`), and linked worktrees inside `W` such as
  `W/.worktree/<name>` together with `W/.git/worktrees/`. Protecting them
  needs the same denies at any depth, which makes `git clone`,
  `git worktree add` and `git submodule update` fail in the sandbox.
- A workspace that is a linked worktree whose gitdir is outside `W`: that
  gitdir is already outside the writable set.
- Other workspace files that tools execute (`.vscode/`, `.idea/`, `.envrc`,
  `Makefile`, `package.json` scripts). Tracked ones appear in `git diff`.

## A failed command's result lists what the sandbox denied

The mechanism follows Claude Code's sandbox-runtime:

- Each Seatbelt driver has a tag, `otto-<32 hex>` (the hex of its private
  directory name). Every deny in the profile, including `(deny default)`,
  carries `(with message "<tag>")`.
- When the driver opens, it starts `/usr/bin/log stream --style ndjson` with
  a predicate on the tag. The driver owns the process and kills it in
  `close`.
- The monitor parses each event's `eventMessage`, which has the form
  `Sandbox: <process>(<pid>) deny(<n>) <operation> <path>` followed by a
  newline and the tag, into operation and path (for a network denial, the
  address). It skips lines that are not JSON; `log stream` writes one such
  header line (`Filtering the log data using …`) first. It keeps at most 256
  entries with their receive time.
- When a command exits with a non-zero code, the driver waits up to 300 ms
  for log delivery. It then attaches to the exit status the denials received
  between the command's start and that point, deduplicated by operation and
  path, at most 20, with a count of the rest. The bash tool appends them after
  stderr:

  ```text
  sandbox_denied:
  file-write-create /Users/me/proj/.git/config
  file-read-data /Users/me/.ssh/config
  [3 more omitted]
  ```

- A command that exits 0 gets no wait and no section.
- The section passes through the existing output redaction.
- Attribution is by time window within one driver: two commands of one
  session that overlap both receive denials from the overlap. A per-command
  tag (`sandbox-exec -D`, see probe item 4) removes this if it proves to
  mislabel results.
- If `log` cannot start or exits (for example when Otto itself runs inside a
  sandbox, where `log` returns "Cannot run while sandboxed"), the driver runs
  without reports and results have no section. One warning goes to the log.

The system prompt states what the section means: for a denied read outside
the workspace, tell the user about `/sandbox allow <path>`; for a denied
write to git metadata or outside the workspace, request `require_escalated`.

## Probe results

The session that wrote this spec runs inside Claude Code's Seatbelt sandbox,
where `sandbox-exec` fails with `sandbox_apply: Operation not permitted` and
`log` refuses to run. The user ran a probe script in a normal shell on
macOS 27.2, on 2026-10-01, against a scratch repository with
`(allow default)`, a workspace `(allow file-write* (subpath W))`, and the five
denies above tagged with `(with message …)`:

1. Rule order. With the denies after the allow rule, writing
   `.git/hooks/pre-commit`, appending to `.git/config`,
   `git config user.email`, creating `.git/commondir` and `mv .git g2` all
   fail with `Operation not permitted`, and `.git/config` is unchanged. With
   the denies before the allow rule, the first five succeed. The denies must
   follow the allow rule.
2. Hard link. With the denies after the allow rule, `ln .git/config hl`
   fails with `Operation not permitted`, so no further rule is needed for
   `.git/config`. A hard link to a file under `.git/hooks` was not tested.
3. With the denies after the allow rule, `git commit`, `git checkout -b`,
   `git worktree add` into a subdirectory of `W`, and writing a workspace
   file succeed.
4. `log stream --style ndjson` started outside a sandbox and delivered one
   event per denied operation (6). An `eventMessage` value:
   `Sandbox: bash(38406) deny(1) file-write-create /private/tmp/otto-probe.D0NRhk/ws/.git/hooks/pre-commit\nOTTO_PROBE_38393`.
   `log` rewrites `eventMessage` in the predicate to `composedMessage` and
   prints a non-JSON header line first.

Not determined: whether `(with message (param "…"))` with `sandbox-exec -D`
works. That check ran after the deny-before pass had renamed `.git`, so the
write failed with `No such file or directory`, and the one matching line was
the `log` header, which contains the predicate text. Part B uses time-window
attribution and does not depend on it.

## Tests

- `profile.rs`: the generated profile contains the five denies for the
  canonical `W` and the message tag, and they come after the workspace
  `allow file-write*` rule.
- Seatbelt conformance (runs in `make check` on macOS): under a real profile,
  writing `.git/config`, `.git/hooks/x` and `.git/commondir`, renaming
  `.git`, and hard-linking `.git/config` fail, and `git commit` succeeds.
- The event parser, against the `eventMessage` value recorded by the probe
  and the non-JSON header line.
- `bash.rs`: a non-zero exit status with denials renders the section; a zero
  exit renders none; the cap, the omission count and redaction apply.
- `excluded_command_entry` refuses `git status`.
- One host test starts `log stream` against a real profile and checks that
  one denial arrives; it skips when `log` cannot start.

## Ownership

- `sandbox/seatbelt/profile.rs` and `testdata/sandbox/profile_v1.sb`: the
  denies and the message tag.
- `sandbox/seatbelt/`: a new `violations.rs` with the `log stream` process,
  the parser and the bounded buffer; the driver attaches denials to the exit
  status.
- `sandbox/mod.rs`: `ExitStatus` gains the denial list; the direct driver
  leaves it empty.
- `tool/bash.rs`: renders the section.
- `otto-core/src/config/sandbox.rs`: `git` in `COMMAND_RUNNERS`.
- `cli/prompt.rs`, the user manual and the README.

## Delivery

Two pull requests. Part A: git metadata denies, `COMMAND_RUNNERS`, prompt,
docs. Part B: denial reports.
