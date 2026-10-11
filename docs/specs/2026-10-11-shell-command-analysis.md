# Shell command analysis with tree-sitter

Status: approved for implementation, 2026-10-11. The user chose both parts
below ("两者都做") and approved the design. Part 1 ships first; part 2 is a
separate PR stacked on part 1.

## Current behavior

- Approval rules match a bash command through
  `otto_core::approval::command_argv`. It runs `is_simple_command` (a
  character scanner), then `shlex::split`. Any `&&`, `||`, `;`, `|`,
  redirect, `$`, backtick or unquoted glob character makes the argv `None`.
  An allow rule never matches such a command. A deny or ask
  `command_prefix` rule does not match it either, so
  `git push origin "$BRANCH"` is not caught by a `["git", "push"]` deny rule.
- Review facts in `tool/bash.rs` use separate tokenizers: `shlex::split` in
  `review_command_target`, `first_push_diff` and `approval_spec_async`; a
  `split_once("&&")` plus regex for a leading `cd`; whitespace splitting in
  `review_facts`. `make&&./run` produces no script facts because the
  token is `make&&./run`. `approval/reviewer.rs` detects `push` with
  `shlex::split`.
- A command with `sandbox_permissions: use_default` produces no approval
  request. Commands such as `git push`, `gh pr merge`,
  `curl -X POST`, `npm publish` reach the network when
  `[sandbox] network = "allow"` (the default). `git reset --hard`,
  `git clean -fdx` and `rm -r` run without a record.

## Part 1: one parser for rules and facts

### Parser

Add `crates/otto/src/tool/shell.rs` using `tree-sitter =0.27.0` and
`tree-sitter-bash =0.25.1`. Both are C code with Rust bindings, so they are
dependencies of `crates/otto` only; otto-core stays wasm32-buildable and
receives already-parsed words.

`shell::analyze(command: &str) -> otto_core::approval::ShellCommand`:

```rust
pub struct ShellCommand {
    /// One entry per simple command found anywhere in the program, in
    /// source order, including commands inside substitutions and compound
    /// statements. Each entry is the command's leading static words: the
    /// unquoted value of every word up to the first word that is not static.
    pub segments: Vec<Vec<String>>,
    /// True only when the whole program is a list of pipelines of simple
    /// commands whose every word is static, with no syntax outside the
    /// accepted set below.
    pub complete: bool,
}
```

`complete` is true only when all of these hold:

- `root.has_error()` is false (tree-sitter-bash accepts malformed input and
  inserts `ERROR`/`MISSING` nodes);
- every node is one of: `program`, `list` (`&&`, `||`), `;` separators,
  `pipeline` (`|`), `command`, `command_name`, `word`, `raw_string`, a
  `string` whose only children are `string_content`, a `concatenation` of
  these, `redirected_statement`/`file_redirect` limited to `2>&1`,
  `>/dev/null`, `2>/dev/null`, `&>/dev/null` (removed from the words);
- no word contains an unquoted `*`, `?`, `[`, `]`, `{`, `}` or a leading
  unquoted `~`;
- no segment's program is in `COMMAND_RUNNERS` (except `git`), contains
  `=`, or is a shell keyword. This matches the current `command_argv`.

Anything else sets `complete = false`. Examples: `$X`, `${X}`, `$(...)`,
backticks, `<(...)`, subshells, `{ ...; }`, `if`/`for`/`while`/`case`,
functions, heredocs, herestrings, other redirects, `VAR=x cmd`, `$'...'`,
`$"..."`, `!`, trailing `&`, comments.

Word values follow bash quoting: `'...'` is literal; inside `"..."` a
backslash escapes only `$`, `` ` ``, `"`, `\` and newline; an unquoted
backslash escapes the next character, and backslash-newline is removed.
Static words are collected even when `complete` is false, so deny and ask
rules can match `git push origin "$B"` by its prefix `["git", "push",
"origin"]`. Parsing is bounded: commands longer than 64 KiB return
`ShellCommand { segments: vec![], complete: false }`.

### Rule evaluation

`otto_core::approval::evaluate_rules(rules, source, tool, command:
Option<&ShellCommand>)` replaces the `&str` argument. `command_argv` is
removed from otto-core; `ShellCommand` is defined in otto-core next to
`ApprovalRule`.

- An `allow` rule with `command_prefix` matches only when `complete` is true,
  there is at least one segment, and every segment starts with the prefix.
  `cargo fmt && cargo test` matches `["cargo"]`; `cargo fmt && rm -r x` does
  not.
- Any `allow` rule (also `source`/`tool` selectors) is skipped when a
  command is present and `complete` is false. This keeps the current rule.
- A `deny` or `ask` rule with `command_prefix` matches when any segment
  starts with the prefix, whether or not `complete` is true.
- deny > ask > allow is unchanged.

Callers: `approval/policy.rs::rule` and the reviewer tests call
`tool::shell::analyze` on the journaled `command` field.

### Review facts

`tool/bash.rs` and `approval/reviewer.rs` read commands only through
`shell::analyze`:

- `review_command_target`: a first segment `cd <dir>` sets the directory for
  the following segments; `git -C <dir>` and `make -C <dir>` are read from
  segment words. A `cd` that is not the first segment, or a `cd` with a
  non-static argument, marks the target unavailable (current behavior for
  unknown targets).
- `review_facts`: script and build detection runs per segment, so
  `make&&./run` reports both `make` and `./run`. `complex_shell` becomes
  `!complete`.
- `first_push_diff` uses the first segment whose words start with `git`
  (after `git` global options `-C <dir>`, `-c <k=v>`, `--no-pager`,
  `--git-dir=<p>`, `--work-tree=<p>`) and whose subcommand is `push`.
- Git facts are collected when any static word is `git` (as before, so
  `sudo git push` marks the git target unavailable). A command that assigns
  a shell variable anywhere (`GIT_DIR=/x git push`) marks the git target
  unavailable, because `GIT_DIR` and `GIT_WORK_TREE` select another
  repository.
- The reviewer treats a command as a push when any static word is `push`, or
  when the command is not `complete` and its text contains `push`
  (`$GIT push`). A push without branch, remote and outgoing-diff facts goes
  to a person.

`is_simple_command` and `[sandbox] excluded_commands` are unchanged. An
excluded command still has to be one simple command; this part does not
widen what runs outside the sandbox.

## Part 2: approval for classified in-sandbox commands

### Classification

`shell::classify(&ShellCommand) -> Option<Effect>` walks every segment and
returns the highest class found. Classification uses the segment's static
words; it does not need `complete`. A segment whose program is not static,
or is a `COMMAND_RUNNERS` entry (`bash -c`, `eval`, `env`, `sudo`, …), is
not classified.

`Effect::ExternalState` (writes outside this machine):

- `git push` unless `-n`/`--dry-run`;
- `gh` subcommands other than read-only ones (`pr|issue list|view|status|diff|checks`,
  `repo view|list|clone`, `run list|view|watch|download`, `release list|view|download`,
  `search`, `status`, `auth status`, `browse`); `gh api` is read-only only
  without `-X/--method` other than `GET` and without `-f`, `-F`,
  `--field`, `--raw-field`, `--input`;
- `curl` with `-X/--request` other than `GET`/`HEAD`, or with `-d`,
  `--data*`, `--json`, `-F`/`--form`, `-T`/`--upload-file`; `wget` with
  `--post-data`, `--post-file`, `--body-data`, `--body-file`, or `--method`
  other than `GET`/`HEAD`;
- `npm|pnpm|yarn publish|unpublish|deprecate`, `cargo publish|yank|owner`,
  `docker|podman push`, `twine upload`, `gem push`;
- `kubectl apply|create|delete|patch|replace|edit|scale|rollout|label|annotate|set|drain|cordon|uncordon|taint`,
  `helm install|upgrade|uninstall|rollback|push`,
  `terraform|tofu apply|destroy|import`.

`Effect::Destructive` (removes workspace or repository content that git
cannot restore):

- `git reset --hard`; `git clean` with `-f`/`--force` and without
  `-n`/`--dry-run`; `git checkout` with `-f`, `--force`, `--` or a `.`
  argument; `git restore` without `--staged` as the only target flag;
  `git switch` with `-f`/`--discard-changes`/`--force`; `git stash drop|clear`;
  `git branch -D` or `--delete --force`; `git reflog expire|delete`;
  `git filter-branch`, `git filter-repo`;
- `rm` with `-r`, `-R` or `--recursive` (including combined short flags such
  as `-rf`); `find` with `-delete`.

All other commands are not classified and run as today.

### Approval

In `BashTool::approval_spec`, when `sandbox_permissions` is `use_default`,
`sandbox_read_path` is empty, the command is not an excluded command, the
tool has approval support (the same condition under which
`require_escalated` is accepted) and `classify` returns a class, return:

| Class | Authority | Title |
| --- | --- | --- |
| `ExternalState` | `Boundary` | `Run a command that changes external state?` |
| `Destructive` | `Bounded` | `Run a destructive workspace command?` |

`source` is `"sandbox"`, `persistent` is false, `reason` is the
justification when given and otherwise names the class and the matched
segment. `details` keeps the existing fields (`command`, `workspace`,
`read_path`, `native_facts`) and adds `"sandboxed": true` and
`"effect": {"class": "external_state" | "destructive", "segment": [...]}`.
`justification` is not required. A `require_escalated` request keeps its
current `Boundary` spec.

After approval the command runs with the sandboxed executor. In
`execute_inner`, a classified `use_default` command runs only when
`authorized` is true (the registry dispatched it through
`execute_approved`); otherwise the tool returns an "approval required"
error and the command does not run. It does not create or consume
`BashApprovals` exact-command grants: those grants are keyed by session and
command text only, and an escalated retry of the same text would consume a
grant given for sandboxed execution. Every production registry that holds
`BashApprovals` also holds the approval service, so `execute_approved` is
the path in practice. Escalation is not granted by this approval.

Existing policy then applies without change:

| Mode | `ExternalState` (`Boundary`) | `Destructive` (`Bounded`) |
| --- | --- | --- |
| allow rule matches | allowed | allowed |
| AUTO | isolated review against user messages | allowed (`bounded-authority`) |
| MANUAL | user decides | user decides |
| NEVER | rejected | allowed (`bounded-authority`) |
| YOLO | allowed | allowed |

A user `command_prefix` allow rule such as `["git", "push"]` approves the
matching requests in every mode, including NEVER. The reviewer requires git
branch, remote and outgoing-diff facts for `git push`; missing facts send
the request to the user, as for escalations today.

The bash tool description (approval variant) states that classified
commands wait for approval and then run inside the sandbox.

## Limitations

- Commands executed by scripts, `make`, `npm run`, `python -c`, `bash -c`,
  `eval`, `xargs`, `env`, `sudo` or similar programs are not classified.
  This is a check on the command text the model writes, not a sandbox
  boundary. Only `[sandbox] network = "deny"` stops network writes from
  those programs.
- Cloud CLIs (`aws`, `gcloud`, `az`) are not classified.
- `Destructive` requests prompt only in MANUAL.
- NEVER rejects `ExternalState` requests that it ran before this change,
  unless an allow rule matches.
- The gate needs the bash tool's approval support, which startup creates
  only with an active Seatbelt sandbox (`runtime/startup.rs`, `elevate`).
  With `--sandbox off` and on Linux no classified command asks.

## Tests

- Parser corpus: quoting and escapes, `&&`/`||`/`;`/`|`, accepted and
  rejected redirects, every rejected node kind, malformed input
  (`has_error`), globs, runners, keywords, 64 KiB bound.
- Rules: allow over compound commands, allow skipped when incomplete,
  deny matching a dynamic command by static prefix.
- Facts: `make&&./run`, leading `cd`, `git -C`.
- Part 2: classification table cases and negatives (`git push --dry-run`,
  `gh pr view`, `curl -I`, `rm file`, `git restore --staged`); approval
  spec authority/title/details; `execute_inner` refuses an unapproved
  classified command and runs an approved one with the sandboxed executor.
