# Sandbox excluded commands

Status: approved 2026-10-01 (step 1 of the sandbox usability plan). Current
behavior is in the user manual's `bash` sandbox section.

## A tool that keeps state in HOME needs a reconfiguration per session

Facts from the code at `135e5ea`:

- A Seatbelt bash command gets a private `HOME`, `TMPDIR` and cache directory
  under `otto-sandbox-<32 hex>/`, created when the driver opens and removed
  when it closes (`crates/otto/src/sandbox/driver.rs:170`, `:632`). Files a
  tool writes under `HOME` are gone after the process exits.
- The profile denies `com.apple.securityd`, so the keychain is unreachable.
- `read_paths` grants read access only.
- `/approve <id>` runs one exact command once, unconfined, with the real
  `HOME` (`compose_workspace_sandbox`, `crates/otto/src/cli/run.rs`). The
  grant does not persist, child agents and `otto --prompt` runs have no
  `/approve`, and the model has to retry the same command after each grant.

`lark-cli` stores its configuration in `~/.lark-cli/config.json`, its app
secret in the keychain, and writes `locks/`, `logs/` and `cache/` under
`~/.lark-cli`. Under the sandbox it fails on every one of these. The user
accepts running it outside the sandbox and wants a general mechanism for
other commands.

## `[sandbox].excluded_commands` runs matching simple commands unconfined

```toml
[sandbox]
excluded_commands = ["lark-cli *", "gh auth status"]
```

Entry syntax:

- `prefix *` matches the command `prefix` alone and any command that starts
  with `prefix` followed by a space or tab.
- Any other entry matches only a command whose text, with leading and
  trailing whitespace removed, is equal to the entry.
- An entry must be non-empty, have no leading or trailing whitespace, contain
  no control character, contain `*` only as the final ` *`, be a simple
  command by the rule below, and appear once. Any other entry makes the
  configuration invalid.

Only a simple command is matched. The command text is scanned once with
shell quoting rules (single quotes, double quotes, backslash outside single
quotes). The command is not simple, and is never excluded, when it has:

- an unterminated quote;
- an unquoted `;`, `&`, `|`, `<`, `>`, `(`, `)`, `#` or line feed. `#` is
  included because quote characters after a comment start are not quotes to
  the shell: `x #'`, a line feed, and `y #'` scan as one quoted word but run
  `y` as a second command;
- a `$` or a backtick outside single quotes, including inside double quotes.

So `lark-cli im +messages-send --text 'a; b'` matches `lark-cli *`, and
`lark-cli auth status && rm -rf x`, `lark-cli $(cat f)` and
`cd x; lark-cli` do not.

A matched command runs through the executor `/approve` uses: the direct
driver with an unconfined filesystem and network, the real `HOME`, and the
same filtered environment (provider API key names removed, sensitive names
only when `allow_env` lists them). Output redaction covers the values in
that environment. The match is checked before `sandbox_permissions`, so a
matching command never asks for approval.

Scope:

- The list exists only in the configuration file Otto loads
  (`~/.config/otto/config.toml` or `--config`). There is no workspace
  configuration, so a repository cannot add entries.
- It applies to the parent session and to child agents, in the REPL, the TUI,
  `otto serve` and `otto --prompt`, whenever the sandbox mode is Seatbelt.
  With `--sandbox off` every command is already unconfined. Where the
  unconfined environment cannot be built with complete redactions, the list
  has no effect and commands stay sandboxed.
- `/sandbox reload` applies a changed list to running sessions, including
  child agents created before the reload.

## Two commands add entries from a running session

- `/sandbox exclude <entry>` validates the entry, appends it to
  `excluded_commands` through the same backed-up writer as `/sandbox allow`,
  and reloads. A reload failure restores the previous file. An entry already
  present is not repeated.
- `/approve <id> always` takes the pending elevated command, requires it to
  be a simple command whose first word has no quote, backslash or `=` and is
  not a program that runs another command (`sh`, `bash`, `env`, `sudo`,
  `xargs`, `python3` and the others in `COMMAND_RUNNERS`), adds
  `<first word> *` the same way, and then grants the pending command once as
  `/approve <id>` does. A command that does not qualify is refused with a
  message to use `/approve <id>`, and nothing is written.

Both are available in the REPL and the TUI, the frontends that have
`/sandbox allow`. The web UI has neither.

The system prompt for a Seatbelt session tells the model that a command the
user listed in `excluded_commands` runs outside the sandbox only when it is
written as one simple command, and that `/sandbox exclude '<program> *'` is
the user's option when a program needs its real `HOME`. The prompt does not
list the entries, so a reload does not change the system prompt.

## Ownership

- `crates/otto-core/src/config`: the field, validation, the simple-command
  scanner, the matcher, the `/approve always` entry derivation, and the
  `update_sandbox` writer (writes the key only when the list is non-empty).
- `crates/otto/src/tool/bash.rs`: `ExcludedCommands` (unconfined executor,
  environment, and the current entries behind a lock) and the routing in
  `BashTool::execute`.
- `crates/otto/src/cli/run.rs`: builds the unconfined executor whenever the
  mode is Seatbelt; `/approve` stays gated on interactive frontends.
- `crates/otto/src/cli/sandbox_switch.rs`: replaces the entries after a
  successful reload.
- `crates/otto/src/app/mod.rs`, `cli/sandbox_setup.rs`, `cli/repl.rs`,
  `tui/`: the two commands.

## Not in this step

- Protecting `.git/hooks` and `.git/config` in the workspace from sandboxed
  writes, and naming the denied path in a failed command's result (step 2).
- Real `HOME` with a credential denylist, and a network proxy with a domain
  allowlist (step 3, a separate design).
- Showing the list in `/sandbox` output, and removing entries from a command;
  edit the configuration file and run `/sandbox reload`.
