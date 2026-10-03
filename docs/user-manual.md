# Otto User Manual

Otto is a local-first agent written in Rust. It turns a
natural-language prompt into a loop of model completions, optional tool calls,
and — when needed — context compaction, in a full-screen TUI, a line-oriented
REPL, or `otto serve`.

This manual describes the behavior implemented by the current build: the
OpenAI-compatible provider and the ChatGPT-subscription provider. It covers only
what the CLI actually does today.

## Contents

1. [Prerequisites](#prerequisites)
2. [Quick start](#quick-start)
3. [ChatGPT subscription](#chatgpt-subscription)
4. [Command-line reference](#command-line-reference)
5. [Environment variables](#environment-variables)
6. [Configuration](#configuration)
7. [Frontends](#frontends)
8. [Slash commands](#slash-commands)
9. [Sessions](#sessions)
10. [Context compaction](#context-compaction)
11. [Tools and safety](#tools-and-safety)
12. [Headless mode](#headless-mode)
13. [Agent server](#agent-server)
14. [ACP agent server](#acp-agent-server)
15. [Chat connector](#chat-connector)
16. [Durable workflows](#durable-workflows)
17. [Memory](#memory)
18. [Skills](#skills)
19. [MCP servers](#mcp-servers)
20. [Troubleshooting](#troubleshooting)

---

## Prerequisites

- macOS, the supported platform: it is the only one with a sandbox, and the
  only one the acceptance gate runs on. Otto also builds and runs on Linux;
  see [platform support](#platform-support).
- The pinned Rust 1.98 toolchain, Node 24+, and `wasm-pack` 0.15 to build from
  source (see [Install from source](../README.md#install-from-source)).
- One of:
  - a reachable OpenAI-compatible endpoint with SSE chat-completions streaming, plus an API key exposed through an environment variable, or
  - a ChatGPT Plus/Pro/Team/Enterprise subscription (see [ChatGPT subscription](#chatgpt-subscription)).

### Platform support

| | macOS | Linux |
| --- | --- | --- |
| `bash` under a sandbox | Seatbelt, the default | none; `auto` and `seatbelt` fail closed |
| `bash` with `--sandbox off` | yes, unconfined | yes, unconfined |
| File tools, sessions, compaction, memory, skills, sub-agents, MCP, workflows, `otto serve` and the Web UI, TUI and REPL | yes | yes |
| Acceptance gate | `make check` | `make check-linux` (no Seatbelt conformance) |

macOS is the supported platform. Otto builds and runs on Linux, but it has no
confined driver there: `auto` and `seatbelt` report `unsupported-platform`,
Otto fails closed, and the `bash` tool is not registered at all. The model
keeps `read`, `grep`, `find`, `ls`, `write`, and `edit`, which stay inside the
workspace as always. `--sandbox off` is the only way to run shell commands on
Linux, and it is exactly as unconfined as it is on macOS: commands run as your
user with your files and your network. Decide that per workspace, not per
habit.

Three host affordances differ. The clipboard behind the TUI's drag-copy is
`pbcopy` on macOS and the first of `wl-copy`, `xclip`, or `xsel` that is
installed on Linux; with none of them, copying reports that no helper was
found. The browser that `otto serve --open` and `otto login` launch is
`/usr/bin/open` on macOS and `xdg-open` on Linux; a launch that fails is never
fatal, because the URL is printed either way. A `bash` command runs under a
login shell (`sh -lc`) on macOS, so it sees the `PATH` `path_helper` assembles
from `/etc/paths` and `/etc/paths.d`; elsewhere it runs as `sh -c`, because
`/etc/profile.d/*` would write into the tool result the model reads and could
restore variables the environment filter removed.

## Quick start

Install the binary:

```bash
make install
```

`make install` builds Otto and otto-connect, installs both to `~/.local/bin/`, and installs
bundled skills under `~/.otto/skills/`; make sure `~/.local/bin` is on your
`PATH`.

For a persistent default profile, run the interactive setup command. It stores
no API key; choose a model ID available to your account or provider. For a
ChatGPT profile, run `otto login` after setup before starting Otto.

```bash
otto setup
```

To configure the file manually instead, create `~/.config/otto/config.toml`:

```toml
default_profile = "deepseek"

[ui]
mode = "auto"

[agent]
shell_timeout = "120s"
max_output_bytes = 51200

[agent.compaction]
auto = true
reserve_tokens = 16384
keep_recent_tokens = 20000

[sandbox]
driver = "auto"
network = "allow"
read_paths = []
allow_env = []

[profiles.deepseek]
provider = "openai-compatible"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key_env = "DEEPSEEK_API_KEY"
```

Export the selected profile's key and start Otto:

```bash
export DEEPSEEK_API_KEY=your-key
otto --config ~/.config/otto/config.toml --profile deepseek
```

An ad hoc run without a config file:

```bash
OTTO_API_KEY=your-key otto \
  --provider openai-compatible \
  --base-url https://api.deepseek.com/v1 \
  --model deepseek-chat \
  --no-session
```

## ChatGPT subscription

Otto can authorize requests with a ChatGPT Plus/Pro/Team/Enterprise subscription
instead of a pay-per-token API key, using OpenAI's "Sign in with ChatGPT" OAuth
flow (the same mechanism the Codex CLI uses).

### Signing in

```bash
otto login
```

`otto login` starts a local callback server, opens your browser to the OpenAI
authorization page, and also prints the URL so you can open it manually if the
browser does not launch. After you approve, it exchanges the authorization code
and writes credentials to `~/.otto/auth/chatgpt.json` with file mode `0600`.

```bash
otto login --status   # report the ChatGPT sign-in state and access-token expiry; exits nonzero if not signed in
otto logout           # remove the stored credentials
```

### Using the subscription

Select the `chatgpt` provider. It requires a `model` but no `base_url` and no
API key:

```toml
default_profile = "chatgpt"

[profiles.chatgpt]
provider = "chatgpt"
model = "gpt-5-codex"
```

```bash
otto --profile chatgpt
```

Or ad hoc, without a profile:

```bash
otto --provider chatgpt --model gpt-5-codex
```

### How it works

- Subscription traffic goes to OpenAI's Responses backend
  (`https://chatgpt.com/backend-api/codex/responses`), authorized by the OAuth
  access token plus the `chatgpt-account-id` header. This is a different wire
  format from the OpenAI-compatible Chat Completions provider, but the CLI,
  tools, sessions, and compaction behave identically.
- The access token is refreshed automatically from the stored refresh token
  when it nears expiry; rotated tokens are written back to the credential file.
- Tokens are never written to TOML, session files, or logs, and are stripped
  from provider error messages.
- Exchanging the login for an API key (which would bill as API credits rather
  than subscription quota) is intentionally not supported.

## Command-line reference

Otto also has subcommands that run before the flags below are parsed:

| Command | Description |
| --- | --- |
| `otto login [--status]` | Sign in with a ChatGPT subscription, or (`--status`) report sign-in state. See [ChatGPT subscription](#chatgpt-subscription). |
| `otto logout` | Remove stored ChatGPT credentials. |
| `otto memory status\|forget <id>` | Inspect or delete memory records. See [Memory](#memory). |
| `otto sandbox setup [--config PATH] [--cwd PATH]` | Choose shell sandbox permissions interactively. See [Interactive sandbox setup](#interactive-sandbox-setup). |
| `otto setup [--config PATH]` | Create an initial provider profile without storing credentials. See [Quick start](#quick-start). |
| `otto workflow run <name> [--input TEXT]` | Start a durable workflow and wait until it finishes or needs approval. |
| `otto workflow status <run-id>` | Print one workflow run and its approval requests as JSON. |
| `otto workflow resume <run-id> [--retry <step-id>]` | Resume safe pending work, or explicitly retry one interrupted step. |
| `otto workflow fork <run-id> --after-step <step-id>` | Create a new run from that step's committed success boundary. |
| `otto workflow approve\|reject <request-id>` | Resolve one persisted workflow approval gate. |
| `otto workflow cancel <run-id>` | Stop active attempts, then mark the run canceled. |
| `otto mcp list` | List MCP servers declared in the configuration file. See [MCP servers](#mcp-servers). |
| `otto mcp add <server> ...` | Add an MCP server to `~/.config/otto/config.toml`. |
| `otto mcp remove <server>` | Remove one configured MCP server. |
| `otto mcp enable <server>` / `otto mcp disable <server>` | Toggle one configured MCP server. |
| `otto mcp login <server>` | Run the OAuth sign-in flow for one configured MCP server. See [MCP servers](#mcp-servers). |
| `otto mcp logout <server>` | Remove the stored OAuth token for one configured MCP server. |
| `otto trust <dir> [--config PATH]` | Record `<dir>` (canonicalized) as a trusted directory in the config file, so `otto serve` admits it and its descendants as workspaces. See [Workspaces](#workspaces). |
| `otto serve [--socket PATH] [--listen HOST:PORT [--open] [--exit-on-stdin-close]]` | Run Otto as an HTTP+JSON+SSE agent server, over a Unix domain socket, a loopback TCP port, or both, instead of an interactive frontend. See [Agent server](#agent-server). |
| `otto acp [--attach [--socket PATH]]` | Run Otto as an Agent Client Protocol v1 agent on stdin and stdout, for ACP clients such as `otto-connect`. With `--attach`, forward each request to a running `otto serve` instead of opening sessions in this process. See [ACP agent server](#acp-agent-server). |
| `otto --attach [--socket PATH] [--resume ID \| --continue]` | Run the TUI as a client of a running `otto serve`, on a session that the web UI and other clients can use at the same time. See [The TUI attached to `otto serve`](#the-tui-attached-to-otto-serve). |

| Flag | Description |
| --- | --- |
| `--help` | Show help and exit. |
| `--config PATH` | Configuration file. Defaults to `~/.config/otto/config.toml`. |
| `--cwd PATH` | Workspace directory. Defaults to `.`. |
| `--profile NAME` | Configuration profile. |
| `--provider NAME` | Provider override: `openai-compatible` or `chatgpt`. |
| `--base-url URL` | Provider base URL override. |
| `--model NAME` | Model override. |
| `--thinking LEVEL` | Model reasoning effort: `low`, `medium`, `high`, `xhigh`, or `max`. |
| `--prompt PROMPT` | Run one prompt without interaction, then exit. `@FILE` reads the prompt from a file (bounded to 1 MiB). |
| `--ui MODE` | Frontend mode: `auto`, `tui`, or `repl`. |
| `--sandbox MODE` | Sandbox driver: `auto`, `seatbelt`, or `off`. `off` is unsafe. |
| `--shell-timeout D` | Shell command timeout (for the `bash` tool). Must be greater than zero. |
| `--max-output-bytes N` | Maximum tool output bytes. Must be greater than zero. |
| `--no-session` | Keep history in memory only; do not persist a session. Cannot be combined with `--continue`, `--resume`, or `--archive`. |
| `--continue` | Continue the newest valid workspace session. Cannot be combined with `--resume`, `--archive`, or `--no-session`. |
| `--resume PATH` | Resume a specific session file; with `--attach`, the session id instead of a path. Cannot be combined with `--continue`, `--archive`, or `--no-session`. |
| `--archive PATH` | Archive one active session file for the current `--cwd`, print the new path, and exit. Cannot be combined with `--continue`, `--resume`, `--no-session`, or `--prompt`. |
| `--socket PATH` | `serve`, `acp --attach`, or `--attach`. Unix domain socket path of `otto serve`. Defaults to `[server].socket`, then `~/.otto/otto.sock`. For `serve` with `--listen`, both listeners are opened. |
| `--listen HOST:PORT` | `serve` only. Listen on a loopback TCP address instead of a socket and print the URL with the access token. Port `0` picks a free port. With `--socket`, both listeners are opened. |
| `--open` | `serve` only. After printing the TCP URL, open it in the default browser (`/usr/bin/open` on macOS, `xdg-open` on Linux). Requires a TCP listener (`--listen` or `[server].listen`). A failed launch is not fatal: the URL is still printed. |
| `--exit-on-stdin-close` | `serve` only. Read stdin and shut down, as on `SIGTERM`, when it reaches end of file or a read fails. Without the flag stdin is not read. Requires a TCP listener. |

## Environment variables

| Variable | Meaning |
| --- | --- |
| `OTTO_PROVIDER` | Provider override (overrides the profile; overridden by `--provider`). |
| `OTTO_PROFILE` | Profile override (overrides `default_profile`; overridden by `--profile`). |
| `OTTO_MODEL` | Model override (overridden by `--model`). |
| `OTTO_API_KEY` | Fallback API key, used when the selected profile's `api_key_env` variable is empty. |
| `OTTO_UI` | Frontend mode (`auto`, `tui`, `repl`); overridden by `--ui`. |
| `OTTO_STARTUP_TRACE` | `1`, `true`, `yes`, or `on` prints a startup timing breakdown to stderr (see [troubleshooting](#a-session-takes-too-long-to-start)). |
| `<api_key_env>` | The variable named by the selected profile's `api_key_env`. Its value wins over `OTTO_API_KEY`. |

API keys are environment variables only. There is no `--api-key` flag, and keys
must never be stored in TOML.

## Configuration

Otto auto-discovers only the global config file at `~/.config/otto/config.toml`.
You can explicitly select any path with `--config`.

Review any explicit config path before you run Otto. Otto never auto-discovers
repository-local config, but an explicit `--config` can request `driver = "off"`,
extra `read_paths`, or `allow_env` grants on the next process start.

```toml
default_profile = "deepseek"

[ui]
mode = "auto"

[agent]
shell_timeout = "120s"
max_output_bytes = 51200
# End-to-end deadlines are disabled when omitted.
# turn_timeout = "30m"
# provider_timeout = "10m"
# subagent_timeout = "1h"
# workflow_step_timeout = "1h"
cancellation_grace = "5s"

[agent.compaction]
auto = true
reserve_tokens = 16384
keep_recent_tokens = 20000

[sandbox]
driver = "auto"
network = "allow"
read_paths = []
allow_env = []

[skills]
enabled = true
paths = ["~/.otto/skills", ".otto/skills"]

[server]
socket = "~/.otto/otto.sock"
# listen = "127.0.0.1:8787"  # loopback TCP instead of the socket
# workspace_roots = ["~/Work"]  # directories another workspace may load from

# Written by `otto trust <dir>`; one table per trusted directory.
# [projects."/Users/me/src/app"]
# trust_level = "trusted"

[failover]
enabled = false
lease_seconds = 30

[profiles.example]
provider = "openai-compatible"
base_url = "https://example.invalid/v1"
model = "gpt-5.6"
thinking = "high"
api_key_env = "EXAMPLE_API_KEY"
context_window = 1050000
compaction_window = 272000
```

Key points:

- `default_profile` names the profile used when no `--profile` is given and no
  session supplies one.
- `[ui].mode` sets the frontend mode (`auto`, `tui`, `repl`).
- `[agent].shell_timeout` and `[agent].max_output_bytes` set default limits.
- `[agent]` also accepts optional end-to-end `turn_timeout`,
  `provider_timeout`, `subagent_timeout`, and `workflow_step_timeout` values.
  Omitted deadlines are unlimited for backward compatibility. A child always
  inherits the earlier of its own configured deadline and its parent's
  remaining deadline; compaction and follow-up work never reset that budget.
  `cancellation_grace` defaults to `5s` and bounds cooperative cleanup before
  Bash or a shared stdio MCP server is force-stopped.
- ChatGPT transport failures before any streamed output are retried up to
  three times, waiting 1, 2, then 4 seconds within the same turn deadline.
  Only the current model request is repeated; previously executed tools are
  not rerun. HTTP errors (including 429/5xx), authorization errors, protocol
  errors, and failures after streamed output are not retried.
  OpenAI-compatible requests are not automatically retried after dispatch.

Deadline checks are cooperative at synchronous filesystem and SQLite
boundaries: Otto checks before and after the call, but does not detach an
in-flight effectful closure or claim a hard wall-clock upper bound for the
system call itself. A timeout or cancellation also does not prove that an
external side effect did not occur.
- `[sandbox]` configures the process-wide shell boundary:

  ```toml
  [sandbox]
  driver = "auto"
  network = "allow"
  read_paths = []
  allow_env = []
  ```

  On macOS, `driver = "auto"` means Seatbelt. Otto never auto-detects or
  auto-falls back to Docker. If Seatbelt cannot be established, Otto fails
  closed by disabling `bash` while keeping `read`, `grep`, `find`, `ls`,
  `write`, and `edit` available. On every other platform there is no confined
  driver at all, so `auto` and `seatbelt` fail closed the same way; see
  [platform support](#platform-support).

  `excluded_commands` (optional list of strings, default empty) names programs
  that run outside the sandbox:

  ```toml
  [sandbox]
  excluded_commands = ["lark-cli *", "gh auth status"]
  ```

  An entry `prefix *` matches the command `prefix` alone and any command that
  starts with `prefix` followed by a space or tab. Any other entry matches only
  a command whose text, with leading and trailing whitespace removed, equals the
  entry. An entry must be non-empty, have no leading or trailing whitespace or
  control character, use `*` only as the final ` *`, be a simple command by the
  rule below, and appear once; otherwise the configuration is invalid.

  Only a simple command can match. A command stays sandboxed when it has an
  unterminated quote, an unquoted `;`, `&`, `|`, `<`, `>`, `(`, `)`, `#`, or
  line feed, or a `$` or backtick outside single quotes (including inside
  double quotes). `lark-cli im +messages-send --text 'a; b'` matches
  `lark-cli *`; `lark-cli auth status && rm -rf x`, `lark-cli $(cat f)`, and
  `cd x; lark-cli` do not.

  A matched command runs through the executor `/approve` uses: no Seatbelt, the
  real `HOME`, network allowed, and the same filtered environment (provider API
  key names removed; sensitive names only when listed in `allow_env`). It runs
  without an approval request. An excluded program has your full user access:
  list only programs you trust with that, such as `lark-cli`, which keeps its
  configuration in `~/.lark-cli` and its secret in the keychain.

  The list is read only from the config file Otto loads
  (`~/.config/otto/config.toml` or `--config`); a workspace cannot add entries.
  It applies to the parent session, child agents, and `otto --prompt` runs
  whenever the sandbox mode is Seatbelt. With `--sandbox off`
  every command is already unconfined. If the unconfined environment cannot be
  built with complete redactions, the list has no effect and commands stay
  sandboxed. `/sandbox exclude` and `/approve <id> always` add entries from a
  running session.

  `/sandbox reload` applies edits to `network`, `read_paths`, and
  `excluded_commands` to the running process (see
  [Slash commands](#slash-commands)). Three changes still need a restart:
  `allow_env`, because the shell environment is fixed when the `bash` tool is
  built; `driver`, because switching between Seatbelt and `off` changes that
  environment; and any change made when the sandbox was already unavailable at
  startup, because there is no `bash` tool to re-point.

- `[skills]` discovers reusable instruction sets from configured roots and
  registers the `skill` tool when at least one skill is found. Config keys are
  `enabled` (default true) and `paths` (default `["~/.otto/skills", ".otto/skills"]`);
  TOML-only, no CLI flags or environment variables.
- `[agent.compaction]` configures automatic context compaction (see
  [Context compaction](#context-compaction)).
- `[server].socket` sets the Unix domain socket path for `otto serve`, and
  `[server].listen` a loopback `HOST:PORT` to use instead (see
  [Agent server](#agent-server)). TOML-only aside from the `--socket` and
  `--listen` flags; no environment variable.
- `[server].workspace_roots` lists directories under which `otto serve` may
  load another workspace besides the startup one (see
  [Workspaces](#workspaces)). Default empty: only the startup workspace is
  admitted. TOML-only, no CLI flag or environment variable.
- `[projects."<path>"]` records a trusted directory: `otto serve` admits it
  and its descendants as workspaces (see [Workspaces](#workspaces)).
  `trust_level` is required and `"trusted"` is its only accepted value; any
  other value fails config loading. `otto trust <dir>` appends the table as
  text, so the rest of the file is kept byte for byte; there is no environment variable
  and no HTTP route that adds one.
- There is no `[inbound]` table. A config that still has `[inbound.feishu]`
  fails to load with an error that says to delete the table. Feishu and
  Telegram messages reach otto through the
  [chat connector](#chat-connector).
- `[failover]` decides whether a session gets a lease directory so it can be
  continued on another host (see
  [Continuing a session on another host](#continuing-a-session-on-another-host)).
  `enabled` (default `false`) applies only to sessions without a lease
  directory, including new ones; a session that already has one uses its lease
  whatever this setting is. `lease_seconds` (default `30`, minimum `12`) is
  written into a new lease directory and cannot be changed for that session
  afterwards. TOML-only, no CLI flag or environment variable.
- Each `[profiles.NAME]` declares `provider`, `base_url`, `model`, and
  `api_key_env`. Optional `thinking` sets that profile's default reasoning
  effort (`low`, `medium`, `high`, `xhigh`, or `max`); omit it to let the
  provider/model choose its default. Optional `context_window` and
  `compaction_window` size proactive compaction for private or unknown model IDs.
- A `provider = "chatgpt"` profile needs only `model`; it ignores `base_url`
  and `api_key_env` and authorizes with the credentials from `otto login`. See
  [ChatGPT subscription](#chatgpt-subscription).
- `[agent].max_turns` is accepted by the schema but no longer limits the agent
  loop. It is not a profile key: every table rejects unknown fields, so
  `max_turns` inside `[profiles.NAME]` fails config loading.

### Precedence

Startup resolution is field-specific:

- **Profile:** explicit `--profile` wins; otherwise a startup `--continue` or
  `--resume` uses the session's stored profile when present, then `OTTO_PROFILE`,
  and a new session uses `OTTO_PROFILE` or `default_profile`.
- **Provider / model:** `--provider` / `--model` override `OTTO_PROVIDER` /
  `OTTO_MODEL`, which override the selected profile and any provider/model
  stored in a resumed session. Explicit `--profile` makes that profile the
  baseline instead, but does not outrank the `OTTO_*` variables.
- **Endpoint:** `--base-url` overrides the profile's `base_url`. There is no
  base-URL environment override, and session files do not supply an endpoint.
- **API key:** the profile's `api_key_env` variable wins if non-empty;
  `OTTO_API_KEY` is the fallback.
- **Agent limits:** `--shell-timeout` and `--max-output-bytes` override
  `[agent]`, which override built-in defaults. These stay in effect across
  in-process `/resume` and `/new`.
- **Sandbox:** `--sandbox auto|seatbelt|off` overrides `[sandbox].driver`;
  otherwise `[sandbox].driver` overrides the built-in `auto`. `network`,
  `read_paths`, and `allow_env` come from `[sandbox]` only. The effective
  sandbox is process-wide and does not change on startup resume or `/new`.
- **Thinking effort:** `--thinking` overrides `[profiles.NAME].thinking`; when
  neither is set, Otto omits the provider reasoning-effort field. In the TUI,
  `/model` opens a profile picker followed by an effort picker; `Enter` applies
  the selected effort for this process and `s` also saves it to the profile.
  `/thinking LEVEL` changes the current session's effort for later requests, and
  `/thinking LEVEL --save` writes it back to the current profile. The effort in
  effect is recorded on the session, so the TUI's `/resume` restores the
  resumed session's own effort; a startup `--continue` / `--resume` resolves it
  from `--thinking` and the profile as above.
- **Agent server listener:** `--listen` > `--socket` > `[server].listen` >
  `[server].socket` > the built-in default `~/.otto/otto.sock`. A `listen`
  value at any level selects TCP and no socket is created. There is no
  environment variable. This applies only to `otto serve`.
- **Agent server workspace admission:** `[server].workspace_roots` and the
  `[projects]` tables only; there is no flag or environment variable, and no
  precedence chain.

Startup `--continue` / `--resume` restore session provider/model only as
defaults; direct flags and `OTTO_*` variables can override them, and a stored
thinking effort is not restored. An in-process
TUI `/resume` restores the selected session's stored provider/model and
thinking effort, and ignores
the process's provider/model/profile/base-URL overrides; its stored profile
selects the endpoint and key environment.

UI mode precedence:

1. `--ui`
2. `OTTO_UI`
3. `[ui].mode`
4. built-in `auto`

Sandbox-driver precedence:

1. `--sandbox`
2. `[sandbox].driver`
3. built-in `auto`

### Backups of the config file

Commands that write the config file — `otto mcp add`/`remove`/`enable`/
`disable`, `otto sandbox setup`, `otto trust`, `/model` (which records the
chosen profile as `default_profile`), `/thinking --save`, `/sandbox allow`,
and `/sandbox network` — first copy the contents they replace into
`backups/` beside the file, named `config-<UTC timestamp>.toml`.
The ten most recent copies are kept; older ones are deleted. Restore one by
copying it back:

```bash
ls ~/.config/otto/backups/
cp ~/.config/otto/backups/config-20260921T143001.417Z.toml ~/.config/otto/config.toml
```

Writes go to a temporary file that is renamed over the config file, so an
interrupted write leaves the previous file intact. Editing the file yourself is
not backed up — Otto only sees the change when it next reads the file.

Each command changes only the key or table it is about in the file's text:
`default_profile`, one profile's `thinking`, one `[mcp.servers.<name>]`
table or its `enabled` key, the `[sandbox]` keys (`excluded_commands` only
when the list is non-empty), or one appended `[projects."<path>"]` table. Comments, blank lines, key order, and every other
table keep their exact bytes. The command then parses the result and compares
it with the intended change; if the target is written in a form it cannot edit
in place, such as dotted keys (`sandbox.network = "deny"`) or an inline table
(`projects = {}`), it fails with a message ending in `the configuration was
not changed` and writes nothing. Rewrite that part as its own `[table]` header
and rerun the command.

Each of these commands reads the whole file, edits it, and writes it back, so
two Otto processes writing at the same time could drop one another's edits. A
write therefore checks that the file still holds what the command read; if
something else changed it in between, the command fails with `the configuration
changed on disk; rerun to apply this change` and the file is left as the other
writer left it. Rerun the command to apply your change on top.

## Frontends

Selection:

- `auto` starts the full-screen TUI only when **both** stdin and stdout are
  terminals. It falls back to the REPL for piped input, redirected output, and
  other non-TTY runs.
- `tui` forces the TUI and fails fast if stdin or stdout is not a terminal.
- `repl` forces the line-oriented REPL even from an interactive terminal.

```bash
otto --ui auto
otto --ui tui
otto --ui repl
OTTO_UI=repl otto
```

### TUI behavior

- Uses the terminal alternate screen buffer.
- Every transcript entry is one block under a marker in the left gutter, so a
  turn reads as prompt, tool calls, reply. The marker opens the block and an
  aligned indent continues it, including on the rows a wrap produced:

  | Marker | Entry |
  | --- | --- |
  | `❯` (bold green on a shaded band) | a prompt you submitted |
  | `⏺` | an assistant reply |
  | `⏺` (cyan) | a tool call; red when it failed |
  | `⎿` | that call's result, indented under it |
  | `✻` (dim) | a compaction checkpoint, a notification, or other system text |

- A prompt is drawn as a shaded band across the transcript width, every row of
  it, so the start of a turn is easy to find when scrolling back. The marker is
  `❯` rather than `>` so it is not mistaken for the `>` a quoted markdown line
  carries in a reply.
- Entries are separated by a blank line.
- While a turn is running, an animated status line is shown under the
  transcript, and the composer title reads `Working — Enter queues for this
  turn · Esc cancels turn` (or, once ordinary input is queued, `Queued for
  next checkpoint · Ctrl+U withdraw · Esc cancels turn`). A queued slash
  command instead reads `Queued next input`, because it remains local until
  the active turn succeeds. Ordinary input is
  delivered within the same turn after the current provider response or tool
  call finishes; `agent_wait` yields immediately. Slash commands still wait
  until the turn finishes. The status line reads
  `PHASE · Ns · turn Ms`: the current phase, the seconds spent in it, and the
  seconds since the turn started. The phase is `waiting for model`,
  `reasoning`, `responding`, `compacting`, or `running TOOL ARGS` (arguments
  truncated to 60 characters). During a ChatGPT transport retry, it shows
  `retry A/M after REASON, waiting Ns`, with a live countdown, followed by
  `requesting` once the delay ends. `A/M` counts retries (1/3 through 3/3);
  the phase and turn elapsed times keep updating. Esc or Ctrl+C cancels the
  retry, including its wait. The Web UI uses the same status text.
- When a thinking effort is set and the provider returns reasoning summaries,
  the summary text streams as a dimmed `reasoning` entry before the reply and
  is saved in the session, so a resumed session shows it again.
- While idle, a pending notification — a finished sub-agent, or a `remind`
  timer — starts a wake turn that delivers it and lets the model continue.
  Esc cancels it the same way as a user turn. Composer text is left in place.
- Assistant responses render as Markdown; if rendering fails, Otto falls back
  to escaped plain text.
- Tool calls are folded to the tool name and a cut-down first line of the
  arguments, with at most the first three result lines under `⎿` and a
  `+N lines (ctrl+o)` count for the rest. A call still running shows no result
  line, and one that finished with none says `(no output)`. `Ctrl+O` shows the
  call id, the arguments, and the output in full, and toggles back.
- The composer keeps a prompt history: `↑`/`↓` walk back and forth through the
  lines already submitted, and `↓` past the newest one restores what was being
  typed. It starts from the prompts the session already had, so a resumed
  session can recall its own, and it is not persisted across runs.
- The composer shows at least 3 input rows even when empty or short, and
  grows with wrapped input up to 12 rows before it scrolls instead of
  growing further.
- `/image <path>` attaches one PNG, JPEG, or WebP image to the next ordinary
  prompt. The path may contain spaces. A later `/image` replaces the pending
  image.
- Mouse-wheel transcript scrolling is enabled. Dragging with the left button
  selects visible transcript text and copies it to the clipboard on release;
  no modifier key is needed. Any key or wheel notch clears the highlight. The
  copied text has the gutter removed — markers and the indent they add — and
  trailing blanks dropped, including the padding a prompt's band adds, so a
  command or a code block pastes as it was written; indentation the text
  itself carried is kept.
- Below the composer, a panel lists this session's sub-agent tasks that are
  queued or running, one row each: a status marker, the task id, its name,
  how long it has been queued or running, and its description. It shows at
  most 4 rows; with more than 4 such tasks the panel shows the first 3 and a
  `+N more` row for the rest. It covers only this session's in-memory tasks,
  not other sessions' or finished tasks — use `/agents` for the full record
  of every session's tasks, running or finished. The panel takes no rows
  when the session has no queued or running task.
- The footer is always the terminal's last row, below the composer and the
  sub-agent panel. It shows profile/model, reasoning effort, the sandbox
  state (`seatbelt · workspace-write · network allowed`, `sandbox off ·
  WARNING: bash is unsandboxed`, or `bash disabled · sandbox unavailable`),
  the workspace, token totals, the context percentage, and the session ID
  when space allows.
- If the terminal is smaller than `40x8`, Otto shows a resize message.

### TUI keys

| Key | Action |
| --- | --- |
| `Enter` | Submit the current prompt, or run the highlighted slash-command suggestion |
| `Tab` | Complete the selected slash-command suggestion |
| `↑` / `↓` | Recall the previous or next submitted prompt, or select a slash-command suggestion |
| `Shift+Enter` / `Alt+Enter` | Insert a newline in the composer |
| `?` | Open the help overlay when the composer is empty |
| `Ctrl+O` | Toggle a tool call between its folded summary and full arguments/output |
| Left-button drag | Select visible text and copy it to the clipboard on release |
| Mouse wheel, `PgUp` / `PgDn` | Scroll the transcript |
| `Home` / `End` | Move the cursor to the start or end of the composer |
| `Ctrl+A` / `Ctrl+E` | Move the cursor to the start or end of the current composer line |
| `Esc` | Cancel the active turn or close the current overlay |
| `Ctrl+C` | Cancel; a second press within one second clears and quits |

### The TUI attached to `otto serve`

```bash
otto --attach [--socket PATH] [--resume ID | --continue]
```

`otto --attach` runs the TUI as a client of a running `otto serve`. Sessions,
models, and tools run in the serve process, so one session can be open in
this terminal, in the web UI, in other `otto --attach` terminals, and in
`otto acp --attach` (the process `otto-connect` starts for chats) at the same
time. The attached process runs no model or tool and reads no provider
credentials. The socket defaults to `[server].socket`, then
`~/.otto/otto.sock`; TCP is not supported. stdin and stdout must be
terminals.

- Without a session flag, it opens a new session in the `--cwd` workspace,
  which `otto serve` must admit (see [Workspaces](#workspaces)).
- `--continue` opens the newest session of that workspace, or a new one when
  the workspace has none.
- `--resume ID` opens the session with that id, as `/session` and the web UI
  show it.
- `--attach` cannot be combined with `serve`, `--prompt`, `--no-session`,
  `--archive`, or `--ui repl`.

At startup it requests `GET /healthz` and opens the session. If either
fails, it prints `otto serve is not reachable at <path>: <error>` to stderr
and exits with status `1`.

Turns:

- A prompt is sent with `POST .../turns` and `"queue": true`, so it waits in
  serve's [turn queue](#turn-queue) while another client's turn runs. The
  prompt appears in the transcript when serve's `user_message` frame for the
  turn arrives. An image attached with `/image` is sent with the prompt; the
  transcript shows only the prompt text.
- Text submitted while a turn runs is sent at once as the next turn, and the
  transcript shows `Queued as the next turn (Ctrl+U withdraws it).`
  `Ctrl+U` cancels the newest such turn that has not started. A slash command
  submitted while a turn runs runs after that turn ends.
- A turn started by another client of the session (the web UI, a chat, or
  another terminal) is shown while it runs: the transcript is reloaded up to
  that turn, then its prompt and events follow. A turn that started and
  finished between two status updates is shown by reloading the history.
- `Esc` cancels the turn being shown, including a turn another client
  started.
- An elevated `bash` command opens the approval dialog when serve reports it
  (see [Approvals inside a turn](#approvals-inside-a-turn)). **Yes** allows
  it and **No** denies it. If another client decides first, the dialog
  closes and the transcript shows `Approval <id> decided elsewhere:
  <decision>`.
- The footer, `/session`, and `/sandbox` show the serve process's session,
  model, and sandbox. The sub-agent panel shows the session's tasks from
  serve, and `/agents` reads serve's task records.

Commands available while attached: `/help`, `/init`, `/session`, `/new`,
`/clear`, `/resume` (the sessions of the workspace), `/rename`, `/compact`,
`/image`, `/context`, `/agents`, `/sandbox`, `/sandbox reload`,
`/approve <id>`, and `/exit`. Every other command prints
`/<command>: not available with --attach` and does nothing: `/model`,
`/thinking`, `/archive`, `/reflect`, `/memory`, `/remember`, `/skill`,
`/tasks`, `/task`, `/timers`, `/mcp`, `/login`, `/logout`,
`/sandbox allow|network|exclude`, and `/approve <id> always`.

If a request to serve fails to connect, or the status stream or a turn's
event stream ends without that turn's `turn_end` frame, the transcript shows
`disconnected from otto serve` and the TUI retries every second. Prompts,
approval answers, `/new`, `/resume`, `/compact`, and `/sandbox reload` sent
in that state are refused with the same line. When serve
answers again, the TUI opens the same session, reloads its history, shows
`reconnected to otto serve`, and follows a running turn again. A turn that
was running when the connection was lost keeps running in serve.

### REPL behavior

- One prompt per line, entered at a `❯ ` prompt — the same marker the TUI
  puts in front of a prompt in its transcript.
- `Ctrl+C` during an active provider call, tool run, or compaction cancels only
  that turn and returns to the prompt; `Ctrl+C` while idle exits with status 130.

## Slash commands

In the TUI, typing `/` opens a filtered suggestion panel; `Enter` executes only
an exact command. In the REPL, type the command and press `Enter`.

Shared commands:

- `/help` shows command help.
- `/init` asks the agent to inspect the repository and create a concise root
  `AGENTS.md` contributor guide. If the file already exists, it is left unchanged.
- `/session` shows session details (ID, path, provider, model, thinking effort,
  and sandbox state, plus the session name once `/rename` has set one).
- `/new` closes the current session and starts a fresh one in the same process.
- `/clear` is an alias for `/new`: it starts a fresh session, so the transcript,
  context counter, and per-session usage reset together.
- `/rename <name>` renames the current session. The new name is written as
  append-only session metadata and is shown in session lists. It is rejected
  while a turn is in flight.
- `/model` shows the current profile/model and effort. In the TUI, bare
  `/model` opens a profile picker followed by a reasoning-effort picker whose
  default selection is the current effective effort; `Enter` applies it for the
  process and `s` saves it to the profile. In the REPL, use
  `/model PROFILE --thinking LEVEL [--save]`.
- `/thinking [LEVEL] [--save]` shows or changes the current reasoning effort.
  Use `unset` or `default` to omit the provider reasoning-effort field.
- `/compact [focus]` creates a manual context checkpoint, or reports
  `[context] no-op` when nothing can be compacted.
- `/sandbox` shows the sandbox state now in effect. `/sandbox reload` re-reads
  `[sandbox]` from the config file and applies it to the running process,
  printing the new state. It is rejected while a turn is in flight, and a
  failed reload keeps the previous sandbox in place. `allow_env` and `driver`
  changes, and a sandbox that was unavailable at startup, still need a restart.
- `/sandbox allow <path>` adds one path to `read_paths` and reloads, so a
  command that the sandbox denied can read it. The path is resolved to an
  absolute, symlink-free path first and must exist; it is written to
  `[sandbox].read_paths` in the config file, so it also applies to later
  sessions. In the TUI the resolved path is shown in a confirmation picker
  whose selected row is `Cancel`; applying it also asks Otto to retry the
  command that failed.
- `/sandbox network allow|deny` sets `[sandbox].network` the same way. In the
  TUI, `/sandbox network` without a mode opens a picker with the current mode
  marked.
- `/sandbox exclude <entry>` appends one entry to `[sandbox].excluded_commands`
  and reloads. The entry is validated first, and an entry already in the list is
  not repeated. Removing an entry has no command: edit the config file and run
  `/sandbox reload`.
- If any of these changes is written but the sandbox rejects it, the configuration
  file is rolled back to what it held before and the previous sandbox stays in
  place.
- `/approve <id>` grants one pending elevated Bash command and immediately asks
  Otto to retry it. The grant is tied to the current session and exact command,
  is consumed once, and remains pending until it is replaced, consumed, or Otto exits.
- `/approve <id> always` first adds `<program> *` to `[sandbox].excluded_commands`
  (see [`[sandbox]`](#configuration)) and reloads the sandbox, then grants the
  pending command once as `/approve <id>` does. `<program>` is the first word of
  the pending command. The pending command must be a simple command whose first
  word has no quote, backslash, or `=`. Otto refuses, and writes nothing, when
  that program runs another command: shells and command runners such as `sh`,
  `bash`, `env`, `sudo`, `xargs`, and `python3`. `git` is also refused: git runs
  programs named by aliases (`alias.<name> = !...`) and by its configuration, so
  `git *` would take arbitrary commands out of the sandbox. Use `/approve <id>`
  for those. Later simple commands that start with the program run unconfined
  with no approval, in this and later sessions. `/sandbox exclude 'git *'` is
  accepted, and it removes the protection described in
  [Git metadata is read-only](#git-metadata-is-read-only-in-the-seatbelt-sandbox)
  for git commands.
- `/skills` lists every available skill name in the current session; use `/skill <name>` for its description, location, contract-check status, and instructions.
- `/skill <name>` displays one skill's description, location, and instructions.
- `/mcp` shows every configured MCP server and its connection state.
  `/mcp login <server>` signs in to one HTTP server that uses OAuth. See
  [MCP servers](#mcp-servers).
- `/login` runs the same ChatGPT sign-in flow as `otto login` without leaving
  the session, and reports `Restart Otto to use the new credentials`;
  `/login status` prints the sign-in state. On a non-`chatgpt` provider it
  answers that there is nothing to sign in to. `/logout` removes the stored
  credentials. See [ChatGPT subscription](#chatgpt-subscription).
- `/agents` lists sub-agent tasks recorded in `~/.otto/tasks.db` by every
  session of every Otto process on this machine, newest first, running or
  finished (see [Sub-agent task records](#sub-agent-task-records)). The REPL
  prints the latest 50 as one line each: parent session, task id, agent,
  status, created time, and the description (or the start of the prompt). The
  TUI opens a modal; see TUI-only commands. `/tasks` and `/task` still cover
  only the current session.
- `/exit` exits when idle (REPL EOF also exits).

TUI-only commands:

- `/context` opens a modal that lists what the next provider request contains:
  the system prompt parts (base, environment, workspace instructions, skills,
  agents), built-in and MCP tool definitions, the compaction summary, the
  previous turn's memory recall, and each message. The title shows the model,
  the estimated total against the context window, the last provider-reported
  input tokens, and the automatic compaction threshold. Token numbers are
  estimates (about 3 bytes per token), not tokenizer counts. `↑`/`↓` select,
  `Enter` expands a section or opens an item's full text (`↑`/`↓`/`PgUp`/`PgDn`
  scroll it), and `Esc` goes back one level. The report is taken when the modal
  opens. The memory section is the previous turn's recall; the next turn
  recalls again with its own prompt.
- `/agents` opens a modal of recorded sub-agent tasks, newest first, with
  status, agent (`default` when none), description, workspace, parent
  session, created time, duration, steps, tool calls, and tokens. It starts
  with this workspace's tasks; `w` toggles between this workspace and all
  workspaces, and `s` cycles the status filter through all, queued, running,
  succeeded, failed, canceled, and interrupted. `↑`/`↓` or `PgUp`/`PgDn`
  select, `Enter` opens the task's prompt, result or error, and child
  transcript (`↑`/`↓`/`PgUp`/`PgDn` scroll it), and `Esc` goes back one
  level. The modal re-reads `tasks.db` every 2 seconds while it is open,
  including while a turn is running. `/agents` can also be opened while a
  turn is running (streaming, or waiting on a sub-agent): typing it and
  pressing `Enter` opens the modal instead of queuing it as the next prompt.
  `Esc` closes the modal (or leaves its detail pane) without cancelling the
  turn; a second `Esc`, with the modal closed, cancels the turn as it always
  does.
- `/image <path>` attaches one image to the next prompt. The image is stored
  inline in the session; the selected model and provider endpoint must support
  image input. Otto sends images with `detail: high`.

- `/resume` opens a modal of the up to 50 most recently modified valid sessions
  for the current canonical workspace. `↑`/`↓` or `PgUp`/`PgDn` to navigate,
  `Enter` to resume, `Esc` to close. It does not search other workspaces. Each
  session id appears once: files that share one (a copy beside the original,
  for example) collapse to the newest-modified file.
- `/archive` opens the same modal to archive a session. `Enter` on a non-current
  session moves it into `archive/` and shows `archived session <id>`. `Enter` on
  the current session archives it and starts a fresh session. `Esc` closes
  without archiving. It is accepted only while idle (a turn, `/new`, or
  `/resume` in progress is rejected) and reports `no active sessions found`
  when the workspace has none. In `--no-session` mode it reports that session
  persistence is disabled.

In the REPL, `/archive` archives the current session and starts a fresh one,
printing the archived path and the new session ID.

## Sessions

Otto writes append-only JSONL in the **Pi session format version 3**, compatible
with the public session format and `SessionManager` API in Pi 0.84.3. Otto
keeps its own storage root, separate from Pi:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

A new session file is created lazily: starting Otto reserves a session, but the
JSONL file is only written on the first user prompt. Starting and quitting
without a prompt leaves no session file behind.

```bash
./otto --cwd /path/to/project --continue
./otto --cwd /path/to/project --resume /absolute/path/to/session.jsonl
./otto --cwd /path/to/project --archive /absolute/path/to/active-session.jsonl
./otto --cwd /path/to/project --no-session
```

Notes:

- `--continue` reopens the newest valid Pi v3 session for the current canonical
  workspace. Invalid files and old Otto v1 files are skipped.
- `--resume PATH` reopens a specific valid Pi v3 session only when its recorded
  workspace matches the current `--cwd`.
- Old Otto v1 files are left untouched, are not listed by `/resume`, and cannot
  be resumed.
- `--no-session` keeps history in memory only.
- **Archiving** moves an active session into a sibling `archive/` directory:
  `~/.otto/sessions/<workspace-key>/archive/<session-id>.jsonl`. The move is
  atomic and preserves the file byte-for-byte with its `0600` mode; nothing is
  deleted and no disk space is reclaimed. The `archive/` directory is created
  `0700` on the first archive and is scoped to that workspace, so archiving one
  workspace never affects another. Archived sessions are excluded from
  `/resume`, `--continue`, and the `/archive` picker, but remain resumable by
  explicit path:
  ```bash
  ./otto --cwd /path/to/project --resume ~/.otto/sessions/<key>/archive/<session-id>.jsonl
  ```
  `--archive PATH` archives one active session for the current `--cwd` and
  exits. It cannot be combined with `--continue`, `--resume`, `--no-session`,
  or `--prompt`.
- **Sub-agent transcripts** are written beside the parent session, in a
  directory named after the parent file without `.jsonl`:
  `~/.otto/sessions/<workspace-key>/<session-id>/<task-id>-<child-id>.jsonl`.
  Each is a Pi v3 session whose header records the parent file in
  `parentSession`; it contains any inherited context, the delegated prompt,
  and every child message and tool call. The file is created on the child's
  first write, with the same `0700` directory and `0600` file modes. Child
  transcripts are not listed by `/resume` or `--continue`. Archiving a session
  moves its transcript directory into `archive/` with it. Under
  `--no-session`, children stay in memory. `/task <id|name>` prints the file
  as a `transcript:` line, and the task JSON carries it as `session_path`. If
  a child transcript cannot be created, only that task fails. A transcript
  starts with an `otto.task_spec` custom entry (task id, name, description,
  model, `context` (`fresh` or `inherit`), and the agent definition if one
  was used) and ends with an `otto.task_result` custom entry (`status`
  `succeeded`, `failed`, `canceled`, or `interrupted`, and the error text). Otto does not
  add these entries to the child's model context. Task ids continue from the
  highest `t<N>` in the transcript directory, so a resumed session does not
  reuse an id.
- **Undelivered notifications** (a finished or reporting sub-agent, a fired
  `remind` timer, a `[feishu]` message) are written to
  `<session-id>.inbox.json` beside the session file, mode `0600`, and removed
  from it once delivered to the model. Reopening the session restores them,
  and the REPL, the TUI, and `otto serve` start a wake turn to deliver them.
  A turn started by user input delivers pending notifications before the
  user's message. Delivery is at least once: a notification appended to the session just
  before Otto exits can be delivered again after the session is reopened.
  Archiving the session deletes the file. A write failure is not reported.
  `--no-session` keeps notifications in memory only.
- When a session is reopened after an exit in the middle of a tool call, the
  first call in the last assistant message that has no result is recorded as
  an error result saying the call may have run; the calls after it are
  recorded as not executed. The model sees these results on its next turn.
- Manual and automatic compaction append Pi v3 `type: "compaction"`
  checkpoints carrying `firstKeptEntryId`, `tokensBefore`, optional usage, and
  bounded file metadata.
- Session files contain sensitive prompt text, responses, summaries, tool
  calls, tool arguments, results, and file metadata. Protect them like source
  data. They do not contain provider API keys, OAuth tokens, authorization
  headers, or cookie values, and Otto does not persist private sandbox profile
  paths as runtime metadata.

### Continuing a session on another host

A session whose directory `~/.otto/sessions/<workspace-key>/` is on a shared
file system can be continued by an Otto process on another host or container
after the first host is lost. Otto allows one writer per session through a
lease in `<session-id>.lease/` beside the session file.

- With `[failover] enabled = true`, a new session creates its lease directory
  on its first write, and opening an existing session without one creates it.
  Once `<session-id>.lease/` exists, every open, resume, and archive of that
  session uses the lease, on every host, whatever that host's `[failover]`
  setting is. There is no command that removes the lease from a session.
- Opening a lease-managed session that another process holds and renews fails
  with `session is held by host <host> pid <pid> (lease epoch <n>)`. When
  the holder exited
  cleanly, the open succeeds at once. When the holder stopped without
  releasing the lease, the open waits 7/6 of `lease_seconds` (35 s with the
  default) without output, then takes the session over.
- On a takeover Otto moves the old log to
  `<session-id>.lease/fenced-<n>.jsonl` and continues from a copy that holds
  only its complete records, so a write from the stopped host after that point
  does not reach the session. Tool calls without results get the error results
  described above, except when every such call names `read`, `ls`, `grep`,
  `find`, or `memory_search`: Otto then runs them again itself, before the
  next model request, as the next attempt of the same operation, and lists
  them in the notification as run again. A call that was already run again
  once and was interrupted again gets the error result instead. Otto then queues one notification to the model that names
  the stopped host and pid, the calls without results (marked "may have run"
  or "not executed"), and each sub-agent task that had not finished, and starts
  a wake turn in the REPL, the TUI, and `otto serve`. With `--prompt`, the
  notification is delivered before the prompt. Each listed task is recorded as
  `interrupted`; a task that cannot be recorded is reported as a warning on
  standard error. No notification is queued when there is nothing to report
  (no call without a result, no unfinished task, and the session ends on a
  finished assistant reply), or when sub-agents are disabled or no provider is
  configured.
- The model continues an interrupted task with the `agent` tool's `resume`
  argument (the task id or name) and a `prompt`. The task keeps its recorded
  agent definition, model, and context; `resume` on a task that is not
  interrupted is an error. A task that is not continued stays interrupted.
  Effects of calls without results are not undone.
- `SIGTERM` to a process that holds a lease (the REPL, the TUI, `--prompt`, or
  `otto serve`) moves the session to the next host that opens it. Otto
  cancels every running turn, tool call, and sub-agent task, waits until each
  agent has written a result for every call, records each task that was
  queued or running as `interrupted`, queues one notification that says the
  session was moved and lists those tasks in the format above, closes MCP
  servers, marks the lease released, and exits `0`. The next open, on any
  host, takes the session at once, without the 7/6 wait and without moving
  the log, and the notification starts a wake turn there. A running tool call
  is cancelled, not waited for; its result says so. No notification is queued
  when no task was cancelled and the session ends on a finished assistant
  reply. A second `SIGTERM` exits at once without releasing the lease, so the
  next host takes the session over as after a lost host. A scheduler must
  allow more time before `SIGKILL` than the cancellation takes; the
  Kubernetes default is 30 s. A process that holds no lease handles `SIGTERM`
  as it did before: `otto serve` shuts down, and the REPL, the TUI, and
  `--prompt` are ended by the signal.
- A process that holds a lease renews it every third of `lease_seconds`. When
  renewal has not succeeded for 5/6 of `lease_seconds`, or another host has
  taken the session over, the process kills the processes its tools started
  and exits with status `75` without writing. Tool calls and log writes are
  refused once the lease is lost.
- On Linux, after each tool call of a lease-managed session Otto runs
  `syncfs(2)` on the workspace's file system before the result is written; a
  sync failure turns the result into an error. macOS has no such sync, so
  `bash` and MCP effects of a call with a result can be missing on the next
  host.
- Requirements and limits: the file system must make data durable when `fsync`
  returns, show another host's synced data to a later `open`, and provide
  atomic exclusive create and atomic rename. This has not been tested on any
  network file system. Writes the stopped host had already handed to its
  kernel can still reach the workspace, `<session-id>.inbox.json`,
  `<session-id>.reminders.json`, and sub-agent transcripts. The lease assumes
  clock-rate differences and process suspensions between hosts stay under a
  third of `lease_seconds`. Workflow runs, memory, usage, and task records
  stay on the host that wrote them. Every host must run an Otto version that
  checks for `<session-id>.lease/`; an older version writes the session with
  only the file lock.

### Optional Pi interoperability probe

If Pi 0.84.3 (or a compatible package exposing the public Pi v3 `SessionManager`
API) is installed, an opt-in probe opens one session and prints bounded JSON
metadata only — never message, summary, or tool content:

```bash
OTTO_PI_INTEROP=1 node ./scripts/pi-session-interop.mjs /tmp/otto-session.jsonl
```

It exits 77 with a `SKIP` message when Pi is unavailable or the gate is unset,
and exits nonzero for an invalid session.

## Context compaction

Compaction is configured in `[agent.compaction]` and, when needed, with
per-profile context metadata.

Rules and defaults:

- `auto = true` is the default. With `auto = true`, Otto does two bounded
  automatic checks:
  - **Proactive compaction:** when the model window is known and the next
    request estimate is above `working_window - reserve_tokens`, Otto attempts
    one checkpoint before sending the provider request.
  - **Reactive overflow recovery:** when the provider returns a typed
    context-overflow error, Otto does one automatic compaction and retries once.
- The automatic paths are one-shot only; they never loop.
- `reserve_tokens = 16384` and `keep_recent_tokens = 20000` are the defaults.
- `context_window` and `compaction_window` must be at least `4096` when set.
- `compaction_window` requires `context_window` and must not exceed it.
- Manual `/compact [focus]` works in both frontends even when `auto = false`.

The optional `focus` text is sanitized, bounded to 8 KiB, and appended only to
the hidden summary-system prompt.

What a checkpoint keeps:

- The model-written summary has an `## Observations` section. The summary
  prompt asks it to list each observed result with its source (tool call,
  command, or user statement), including failed results, and to record user
  corrections as `user: ...`. It asks the model to carry every observation from
  the previous summary forward and to mark a contradicted one
  `(superseded by: ...)` rather than delete it. A summary without the section is
  rejected. Otto checks that the section is present, not what it contains.
- The text of every summarized user message is appended to the summary
  verbatim, one JSON string per line, in a `<user-messages>` block. Otto builds
  this block itself, so the summary model cannot change or drop its lines. The
  block carries forward across later compactions. When it would exceed 16 KiB,
  the oldest messages are dropped. A message longer than 2,000 characters is
  cut and marked `[user message truncated for compaction]`.
- The paths the summarized tool calls read and modified are appended in
  `<read-files>` and `<modified-files>` blocks.

### Model window metadata

Otto ships a static limit catalog for common GPT, o-series, and Claude model IDs
so it can size compaction conservatively.

- Listed full-size GPT-5.4/5.5/5.6 aliases use `context_window=1050000`,
  `hard_input_window=922000`, and `compaction_window=272000`.
- `gpt-5.4-mini`, `gpt-5.4-nano`, GPT-5/5.1/5.2 aliases, and `gpt-5.3-codex` use
  the 400K catalog family with a 272K working window.
- `gpt-5.3-codex-spark` is an exact 128000-context / 32000-output override;
  `*-chat-latest` aliases use their catalog chat values.
- GPT-4.1, GPT-4o, o1/o3/o4, and listed Claude aliases use the static catalog.
- Claude metadata is used only when an OpenAI-compatible endpoint exposes a
  Claude-family model ID; Otto has no Anthropic provider.

If a model ID is unknown, proactive automation is disabled (no trustworthy local
window), but reactive one-shot recovery can still happen after a typed provider
overflow. For private deployments, set `context_window` and optionally
`compaction_window` on the selected profile.

## Tools and safety

Startup reads `AGENTS.md` (or `CLAUDE.md`) through the workspace directory
handle and includes at most 8 KiB. Links outside the workspace and non-regular
files are skipped. Automatic Git status queries use the configured sandbox
executor with fsmonitor disabled; unavailable executors produce no Git status.

`SKILL.md` and `AGENT.md` must stay within their respective skill or agent
definition directory. Configured external roots and linked definition
directories remain supported.

### File tools

The six file tools are always enabled and always restricted to the initial
canonical workspace, even when `--sandbox off` is selected:

- `read` reads UTF-8 text with optional line offsets and limits; files larger
  than 64 MiB are rejected before being read into memory.
- `grep` searches file contents with RE2-style regular expressions
  (case-insensitive matching, optional `**` glob filtering, up to 100 matches by
  default, 1000 maximum).
- `find` returns sorted regular-file paths matching `**` globs (up to 1000 by
  default, 10000 maximum).
- `ls` lists one directory level in sorted order; directories end in `/` and
  symlinks in `@`.
- `write` writes a complete file atomically and syncs the file and its
  directory to disk before it reports success.
- `edit` replaces one or more unique text matches and shares the 64 MiB size
  limit with `read`. When `old_text` has no exact match, `edit` retries with a
  match that ignores trailing whitespace and treats curly quotes, dashes, and
  non-breaking spaces as ASCII, and it rewrites only the part of `old_text`
  that `new_text` changes.

Recursive `grep` and `find` skip `.git` and discovered symlinks but include
other dotfiles. They also skip what the workspace's `.gitignore` files exclude,
so a search is not spent on build output or vendored dependencies; pass
`no_ignore: true` to search those files anyway. Pointing `path` at an ignored
directory searches it, because naming it is the request. The rules honored are
git's own, including `!` negation, directory-only trailing `/`, anchoring
slashes, and `**`; `.git/info/exclude` and the global `core.excludesFile` are
not read. Binary files, invalid UTF-8 files, and files with lines larger
than 1 MiB are skipped by `grep`. Otto canonicalizes paths, resolves symlinks,
and rejects workspace escapes. Actual file operations use a directory handle
so replacing a path during an operation cannot redirect them outside the
initial workspace.

### `remind`

`remind` schedules a later wake. It returns immediately and reports the id
it assigned, such as `scheduled r1 in 10s: check the build`. When the delay
elapses, Otto delivers a `[timer]` notification and the idle wake loop
starts a turn, the same way a finished sub-agent does. At most eight timers
can be outstanding. Each delay is 1 to 3600 seconds.

`remind_status` lists the outstanding timers of the current session, one per
line, sorted by fire time: the id, the remaining time (`due` once the fire
time has passed), and the message. `remind_cancel` takes an `id` from that
list and cancels that timer. Both answer `no timers in this session` and
`unknown timer: <id>` respectively.

In the REPL and the TUI, `/timers` prints the same list and
`/timers cancel <id>` cancels one timer. `otto serve` exposes the same two
operations as `GET /v1/sessions/{id}/timers` and
`POST /v1/sessions/{id}/timers/{timer_id}/cancel`.

File-backed sessions keep outstanding timers across a restart; opening that
session restores them, and a timer that is already due fires as soon as Otto
is idle. A timer that fired just before Otto exited can fire again after the
restart. `/new` starts a different session without them. `--no-session`
timers live only in the current process. Archiving a session cancels its
outstanding timers permanently: the stored timers are deleted with the
archive, so resuming the archived session does not bring them back. Child
agents do not get these tools.

### `list_models`

With the `openai-compatible` provider, `list_models` returns the model ids the
endpoint reports at `GET {base_url}/models`, sorted, one per line. Otto sends
the request itself, so the API key stays in the Otto process and never reaches
`bash`. The request is made once, with no retry; a body larger than 8 MiB or
one without a `data` list is an error.

The system prompt tells the model to take model ids from `list_models`, both
when it writes one into a reply or a configuration file and when it picks a
`model` for a sub-agent. The `chatgpt` provider has no `list_models`; there the
model is told not to write a model id from memory and to use an id you named or
the session's model. Child agents do not get this tool.

### Interactive sandbox setup

Run `otto sandbox setup` to choose network access and optionally add the built-in
GitHub CLI recipe. Use `--config PATH` for another configuration file and `--cwd
PATH` to select the workspace used by the check. No model or provider login is
required.

The wizard shows the proposed permissions before saving. It enables Seatbelt,
preserves existing extra permissions and unrelated TOML content, and changes only
the sandbox table. Cancel or end input to leave the file unchanged. Configuration
uses a separate `[sandbox]` table; unsupported layouts are rejected without edits.
Changes apply to future processes using that config file, not just the selected
workspace.

The GitHub CLI recipe exposes its configuration directory read-only and allows
`GH_CONFIG_DIR`. This can expose saved GitHub credentials to shell commands. The
wizard uses an existing absolute `GH_CONFIG_DIR`, or defaults to `~/.config/gh`,
and prints a launch command setting that variable because Otto replaces `HOME`.
Run `gh auth login` outside Otto first if the configuration directory is missing.

Choose `check` to test sandbox startup and, when selected, GitHub CLI availability
and directory access using the displayed launch environment. The check does not
contact GitHub or verify authentication or network connectivity. Choose `save`
to write the reviewed configuration, then restart Otto with the printed command.

### `bash` sandbox policy

On macOS, the default is `--sandbox auto`, which means Seatbelt. The sandboxed
command gets whole-workspace write access plus Otto-managed private `home`,
`tmp`, and `cache` directories beneath your user cache. Otto keeps generated
profile files in a separate private `profiles` directory that the sandboxed
child cannot read. Otto does not treat the workspace as protected: source,
`.git`, tests, and generated files remain writable.

Host home content is not automatically readable. Git config, shell dotfiles,
and host caches are not implicitly mounted into the command view. Add only the
narrow absolute or `~/...` `read_paths` you need. Broad `read_paths` are high
risk because command code can read them and, with `network = "allow"`, exfiltrate
them. Otto rejects `read_paths` that would include Otto's private sandbox state.
If you need tool-specific config, prefer narrow `read_paths` plus exact config
environment variables over exposing a large home or cache subtree.

`network = "allow"` is the default and permits ordinary IP networking and local
IP binds. `network = "deny"` blocks IP networking and local binds. Phase 1 has
no domain allowlist. Unix sockets stay blocked in both modes, except for the
exact `/private/var/run/mDNSResponder` path, which `network = "allow"` permits
because `getaddrinfo` connects it directly to resolve hostnames. Docker/Podman
sockets, SSH agents, and similar host control sockets remain unavailable.
`network = "allow"` also permits lookups of the `com.apple.trustd` and
`com.apple.trustd.agent` services, which the Security framework uses to verify
TLS server certificates; without them a client that verifies certificates
through that framework fails with an opaque trust error such as
`x509: OSStatus -26276`.

The command environment is rebuilt from one captured process snapshot. Otto
never restores provider API-key variables, `OTTO_API_KEY`, loader-injection
variables, shell-startup injection variables, `SSH_AUTH_SOCK`, or Otto's own
sandbox variables. `allow_env` restores only exact names after filtering and is
high risk because it grants the restored value to untrusted command code.
Restored values are still added to Otto's exact-value redactor.

If Otto would need to retain more than 512 sensitive values or more than 1 MiB
of sensitive-value bytes for exact redaction, it fails closed by disabling
`bash` for that process. Exact-value redaction is defense in depth only: if a
command transforms or encodes a secret, Otto may not be able to redact it.

If you explicitly select `--sandbox off`, Otto prints a persistent local warning
and `bash` runs unsandboxed as your current user. In that mode,
`network = "deny"`, private-home/cache replacement, and `read_paths` no longer
constrain the shell.

In an interactive parent session using Seatbelt, the model may set
`sandbox_permissions` to `require_escalated` and provide a justification. Otto
does not run the command; it returns an approval ID. Review the exact command
and reason, then enter `/approve <id>`. The matching command runs once through
the existing unconfined driver with the same filtered environment rules as
other Bash commands. Approval is never automatic, is unavailable to child
agents and one-shot `--prompt` runs, remains pending until it is replaced,
consumed, or Otto exits. `/approve <id>` does not modify sandbox
configuration; `/approve <id> always` also adds the program to
`[sandbox].excluded_commands`.

Commands matching `[sandbox].excluded_commands` run through the same unconfined
driver without an approval. They apply to child agents and `otto --prompt`
runs as well, and only to simple commands; see
[`[sandbox]`](#configuration) for the entry syntax and the rule.

### Git metadata is read-only in the Seatbelt sandbox

Git runs hooks and the programs named in its configuration outside the
sandbox, so a sandboxed command must not write them. Under Seatbelt, these
paths are read-only, whether or not they exist, and no setting turns this off:

- `<workspace>/.git` (the entry itself, so it cannot be renamed or removed)
- `<workspace>/.git/config`
- `<workspace>/.git/config.worktree`
- `<workspace>/.git/commondir`
- `<workspace>/.git/hooks` and everything below it

The rule applies to the workspace root only. Repositories below it (nested
clones, submodules, and linked worktrees inside the workspace) are not covered.

Commands that fail in the sandbox because they write these paths: `git init`
in the workspace, `git config` without `--global`, `git remote add` and
`set-url`, `git push -u`, `git branch --set-upstream-to`, creating a branch
that tracks a remote branch, `git submodule init`, and hook installers such as
`pre-commit install`.

Commands that are not affected: `git commit`, `checkout`, `branch` without
tracking, `fetch`, `pull`, `merge`, `rebase`, `stash`, `worktree add`,
`clone` into a subdirectory, and `git config --global` (which writes the
private HOME).

To run one of the failing commands, the model sets `sandbox_permissions` to
`require_escalated`, and you enter `/approve <id>`.

### A failed command lists what the sandbox denied

When a `bash` command exits non-zero or is killed by a signal, its result can
include a `sandbox_denied:` section between the stderr block and the
`exit_code:` line. Each line is one refused operation and its target, for
example `file-read-data /Users/me/.ssh/config` or
`file-write-create /path/to/workspace/.git/hooks/pre-commit`. The system
prompt instructs the model to answer a denied read outside the workspace by
suggesting `/sandbox allow <path>`, and a denied write to `.git` metadata or
outside the workspace by requesting `require_escalated`.

Limits:

- Only commands with a non-zero exit code or a signal get the section. A
  command that exits 0 is not delayed and has none.
- Otto waits 300 ms after the exit for the macOS log to deliver events, so a
  denial that arrives later is missed.
- At most 20 distinct (operation, target) pairs are listed, followed by
  `[N more omitted]` when there are more. The driver keeps the latest 256
  events; older ones are dropped.
- Denials are attributed to a command by time window. Commands of one session
  that run at the same time can show each other's denials.
- The section needs `/usr/bin/log`, which does not run when Otto itself runs
  inside a sandbox. In that case no section appears and nothing else reports
  it.
- Commands that run outside the sandbox (`--sandbox off`, excluded or approved
  commands) have no section.

### Seatbelt limitations

Otto depends on Apple's deprecated `/usr/bin/sandbox-exec`. It improves
command isolation on macOS, but it is not a VM boundary. Otto does not claim
protection against same-user or same-kernel attacks, pre-existing hard links,
`setsid` escaping Otto's process-group cleanup, resource exhaustion, or
intentional damage inside the writable workspace. Docker and Apple Container are
not detected or supported.

Default limits remain a 120-second shell timeout and a 50 KiB tool-output cap.
Override them with `--shell-timeout` and `--max-output-bytes` or `[agent]`.

## Headless mode

`--prompt` runs a single prompt without interaction and exits: `0` on success
or after a `SIGTERM` that moved a lease-managed session (see
[Continuing a session on another host](#continuing-a-session-on-another-host)),
`1` on error, `130` on interrupt. The value is the prompt text, or `@PATH` to
read the prompt from a file (bounded to 1 MiB).

```bash
./otto --prompt "summarize TODOs in this repo"
./otto --prompt @prompt.txt --no-session
./otto --prompt "explain main.go" --thinking max --continue
```

`--prompt` cannot be combined with `--ui tui` or `--archive`, and composes with
`--continue`, `--resume`, and `--no-session`.

## Agent server

`otto serve` runs Otto as a long-lived HTTP+JSON+SSE frontend, instead of the
TUI or REPL. One process holds the startup workspace (`--cwd`, default `.`)
and, when `[server].workspace_roots` or a trusted directory admits others, any number of additional
workspaces loaded on first use; it manages any number of sessions across all
loaded workspaces. Turns in different sessions run concurrently. A session
runs one turn at a time; a turn requested while another runs is queued when
the request asks for it, and otherwise returns `409` (see
[Turn queue](#turn-queue)). It listens on a Unix domain socket (the default),
a loopback TCP port, or both.

```bash
otto serve [--socket PATH] [--listen HOST:PORT [--open]]
```

`serve` accepts the same startup flags as the interactive frontends
(`--config`, `--cwd`, `--profile`, `--provider`, `--base-url`, `--model`,
`--thinking`, `--sandbox`, `--shell-timeout`, `--max-output-bytes`) plus
`--socket` and `--listen`, and `--open` to launch the printed TCP URL in the
default browser. It rejects `--ui`, `--prompt`, `--resume`, `--continue`,
`--archive`, and `--no-session`. `--open` requires a TCP listener.

### Listener

With both `--socket` and `--listen` on the command line, Otto opens both
listeners and serves the same sessions on each: requests on the socket carry
no token, requests on the TCP port need the token. A browser uses the TCP
URL; local clients can use the socket at the same time. Otherwise one
listener is opened, resolved in this order: `--listen` > `--socket` >
`[server].listen` > `[server].socket` > the built-in default
`~/.otto/otto.sock`. The config file cannot select both.

**Unix socket.** Otto creates a missing parent directory with mode `0700`,
creates the socket file with mode `0600`, and refuses to start if a live
server already owns that path. A socket file left by a process that did not
run the normal shutdown, for example after `SIGKILL` or a second `SIGTERM`,
is replaced at the next start. File permissions are the only access control;
requests carry no token.

**Loopback TCP.** `--listen HOST:PORT` accepts only loopback hosts:
`127.0.0.1`, `::1`, or the literal `localhost` (mapped to `127.0.0.1` without a
DNS lookup). Any other host is rejected at startup. Port `0` picks a free
port. Otto generates a random access token for the process and prints one
line to stdout before serving:

```
otto serve: http://127.0.0.1:PORT/?token=<token>
```

`--open` then launches that URL with `/usr/bin/open` on macOS and `xdg-open`
on Linux. A failed launch is not fatal. `--open` without a TCP listener exits
with `otto: --open requires a TCP listener`.

Every `/v1/` request must then carry `Authorization: Bearer <token>`; a
missing or wrong token returns `401` with `WWW-Authenticate: Bearer`. The
token is accepted from that header only, never from a query parameter. `/`,
`/assets/`, `/healthz`, and `/metrics` need no token. The token is never
logged and is not persisted; restarting the process issues a new one. There
is no TLS and no CORS: the port is meant for a browser or client on the same
machine.

### Workspaces

The startup workspace is loaded when the process starts. `[server].workspace_roots`
(default empty) lists directories under which another workspace may be
loaded on first use, from a session create naming it or from
`POST /v1/workspaces`. A path is admitted when it is absolute and, after
resolving symlinks and canonicalizing it, it is an existing directory that is either the startup
workspace, a descendant of one canonicalized root (a root itself is
admitted), or a trusted directory or its descendant. An unadmitted path returns `403` with `code: "WORKSPACE_NOT_ADMITTED"`;
a relative, missing, or non-directory path returns `400` with
`code: "INVALID_WORKSPACE"`.
With the default empty `workspace_roots` and no trusted directories, only
the startup workspace is admitted and the server behaves as a
single-workspace process.

Trusted directories come from the config file's `[projects]` tables, which
`otto trust <dir>` writes:

```bash
otto trust ~/src/app   # prints: Trusted /Users/me/src/app.
```

`otto trust` requires an existing directory and stores its canonical path.
Running it again for a directory already listed writes nothing. The write
takes a backup and is refused if another process changed the file in
between (see [Backups of the config file](#backups-of-the-config-file)).
A running `otto serve` re-reads `[projects]` on every admission, so a
directory trusted after it started is admitted without a restart. A trusted
path that no longer resolves is skipped. If the config file cannot be read
or parsed at that moment, no trusted directory is admitted until it can.
Trust affects admission only; the sandbox and approval policy are the same
for every workspace.

- `GET /v1/workspaces` lists loaded workspaces, startup first, then by path:
  `{"startup": path, "roots": [path...], "workspaces": [{"path", "open_sessions", "workflows"}...]}`.
- `POST /v1/workspaces {"path": "..."}` admits and loads a workspace,
  returning one `workspaces` entry: `201` when it was newly loaded, `200`
  when it was already loaded. With `"trust": true`, a path refused as
  `403 WORKSPACE_NOT_ADMITTED` is first recorded as trusted in the config
  file, as `otto trust` does, and then loaded; an already admitted path
  records nothing. A path that is not an existing directory is still `400`.
- `GET /v1/fs/dirs?path=...` lists the subdirectories of an absolute
  directory for the web UI's folder picker:
  `{"path", "parent", "roots": [path...], "dirs": [{"name", "path"}...]}`.
  Listing is limited to the home directory, the `workspace_roots`, and their
  descendants, checked after resolving symlinks; without `path` it lists the
  home directory. Only directory names are returned, names starting with `.`
  are left out, and `parent` is `null` at the top of a root. `400
  INVALID_PATH` for a relative path or a non-directory, `403
  PATH_NOT_ALLOWED` outside those directories.
- `POST /v1/sessions` takes an optional `"workspace"`; the default is the
  startup workspace. `GET /v1/sessions` and `GET/POST /v1/workflows` take an
  optional `?workspace=`; absent, `GET /v1/sessions` covers every loaded
  workspace, and the workflow list and start use the startup workspace.
  Run-scoped workflow routes (`GET /v1/workflows/{id}`, `.../resume`,
  `.../fork`, `.../cancel`, and the approval routes) search every loaded
  workspace for the run or request id. Every workspace value is admitted
  before any lookup, so an unadmitted path is `403` even for a read.
- `POST /v1/sandbox/reload` reloads every loaded workspace's sandbox and
  keeps the single-workspace `409` guard: it fails while any open session in
  any loaded workspace has a turn running, and also fails `409` if any
  workspace's reload itself errors. With one loaded workspace the response is
  unchanged, the reloaded sandbox object; with more than one, it adds a
  `workspaces` array with each workspace's own reload result.
- Each loaded workspace with a workflow database keeps its own file lock
  (`~/.otto/workflow-locks/{key}.lock`); a workspace whose lock is held by
  another process has workflows disabled for it (`GET/POST /v1/workflows`
  returns the same error a single-workspace server returns when workflows
  are disabled), while its sessions and turns still work.
- `DELETE /v1/workspaces?path=...` unloads a workspace and removes it from
  the persisted list: `204` on success, `404 WORKSPACE_NOT_FOUND` when it is
  neither loaded nor listed, `409 WORKSPACE_IS_STARTUP` for the startup
  workspace, and `409 WORKSPACE_IN_USE` while a session in it is open or a
  workflow run in it is active. A listed path whose directory no longer
  exists can still be removed. Nothing on disk under the workspace changes.
- A workspace newly loaded by any request that names it
  (`POST /v1/workspaces`, or a `workspace` on a session or workflow request)
  is added to `~/.otto/serve-workspaces.json` (`{"workspaces": [path...]}`,
  sorted, one file shared by every `otto serve` process). The startup
  workspace is not written. At startup, after the startup workspace, each
  listed path is admitted against the current `workspace_roots` and loaded;
  a path that is missing, not admitted, or fails to load is skipped with a
  `warning:` line on stderr and stays in the file. An unreadable or
  unparsable file is a warning and an empty list. If the file cannot be
  written, the workspace stays loaded and stderr gets
  `warning: cannot save workspace list: <error>`. `DELETE /v1/workspaces`
  removes an entry.

### Turn queue

`POST /v1/sessions/{id}/turns` with `"queue": true` starts the turn at once
when the session has no running or queued turn and no compaction, and
otherwise appends it to the session's queue with status `queued`. Queued
turns start one at a time in arrival order, each after the previous turn
ends. A session queues at most 16 turns; the 17th request returns
`409 queue_full`. Without `"queue": true` the request returns
`409 turn_active` while a turn runs or is queued.

A queued turn has an id and can be read, followed and cancelled through the
turn routes. Its event stream sends nothing until it starts. Cancelling it
removes it from the queue and ends it `canceled`. A server-started wake turn
(`trigger` `task`) starts only when no turn is running or queued.
`POST .../compact` returns `409 turn_active` while a turn runs or is queued.
On shutdown, queued turns end `canceled` without starting.

Each `POST .../turns` response carries the header `Otto-Turn-Id` with the
turn's id. In the event stream of a turn started by a client, the first frame
is `user_message` with the prompt `text`, and `image: true` when an image was
attached (the image data is not repeated). Wake turns have no `user_message`.
The last frame of every turn is `turn_end` with `status` (`ok`, `error`, or
`canceled`) and, for `error`, the error text. Both frames are kept in the
turn's event buffer, so a reader that resumes with `?after=N` receives them
as well. Turn errors are redacted of configured secrets before they are
stored, returned, sent in `agent_error` or `turn_end`, or logged.

### Approvals inside a turn

When a step of a serve turn ends with an elevated Bash command waiting for
approval, the turn does not end. Its stream emits `approval_requested` with
`approval_id`, `tool_call_id`, `command`, and `justification`, and the turn
stays `running` while it waits; other turns queue behind it.
`POST /v1/sessions/{id}/approvals/{approval_id}` with `{"decision":"allow"}`
or `{"decision":"deny"}` decides it, from any client:

- The first decision returns `200 {"decision": ...}`. A later decision for
  the same request returns `409 approval_decided`; an id that is not waiting
  returns `409 approval_failed`.
- `allow` grants the command once and the server retries it inside the same
  turn, with the same turn id and stream. The client sends no prompt.
- `deny` ends the turn with status `ok`.
- A request with no decision after 10 minutes is denied.
- Each outcome emits `approval_decided` with `approval_id` and `decision`
  (`allow`, `deny`, or `timeout`). Cancelling the turn during the wait ends
  it `canceled`.

Excluding the command's program from the sandbox (the TUI's
`/approve always`) is not available through the API.

### Web UI

`GET /` serves the browser UI built by `make ui` and embedded into the
binary, and `GET /assets/` its static files. `make build` refreshes the UI
first; a direct `cargo build` without running `make ui` answers `/` with the
plain-text line `Web UI not built; run make ui`.
`--open` opens that URL in the default browser; otherwise open the printed
URL yourself. The page moves the token from the query string into the tab's
`sessionStorage`, removes it from the address bar, and sends it as the
`Authorization` header on every API call. Closing the tab discards it; open
the printed URL again to get back in.

In the Chat view the page has a sessions sidebar, the transcript, and a
composer:

- The sidebar groups sessions (`GET /v1/sessions`) by working directory,
  the session's `workspace`. Groups are the loaded workspaces
  (`GET /v1/workspaces`) plus any directory a listed session reports; the
  startup workspace comes first, the rest by path. A group header shows the
  directory basename, with the full path in a tooltip. Each row shows the
  session name, `●` when the session is open on the server, and the model;
  the current session is highlighted.
- The sidebar reads `GET /v1/status` and marks open sessions with badges:
  `running` while a turn runs, `approval` while an elevated Bash command
  waits for approval, `error` when the last turn failed, and `N tasks` while
  sub-agent tasks are queued or running. A group header shows how many of its
  sessions are running. The page reconnects 1s after the stream ends or
  fails, except after a `401`, which it shows as an error; it
  re-reads the session list when a status names a session it does not list.
- Selecting a session opens it with `POST /v1/sessions {"resume": id}` and
  renders its history. The session id is kept in the URL fragment, so a reload
  reopens the same session.
- Each group's **New session** button creates a session in that directory;
  `"workspace"` is sent only for a non-startup directory. `/new` keeps
  creating in the currently open session's workspace.
- Each group's **Changes** button opens the Changes view for that directory
  (`GET /v1/workspaces/diff`): the branch, then one collapsible entry per
  changed file with its status and patch. It fetches again on **Refresh** and
  when a session in that directory finishes a turn. It is read-only.
- Each group other than the startup workspace has a **Remove** button, which
  calls `DELETE /v1/workspaces`; a `409` message is shown next to the
  **Add workspace…** button.
- **Add workspace…** at the bottom of the sidebar opens a folder picker. In
  a browser it browses the server's directories through `GET /v1/fs/dirs`;
  in the desktop app it is the native macOS folder dialog. The chosen folder
  is registered with `POST /v1/workspaces` and appears as a group. If the
  server answers `403 WORKSPACE_NOT_ADMITTED`, a **Trust this folder?**
  dialog shows the path; **Trust and add** repeats the request with
  `"trust": true`, and **Cancel** adds nothing. **Enter a path** below the
  button expands a field for typing an absolute path, handled the same way.
  Other `400`/`500` errors are shown below the button.
- Below 720px wide the sidebar is hidden; the **Sessions** button in the top
  bar shows it as an overlay, and opening a session hides it again.
- Typing `/` in the composer shows local suggestions for supported Web slash
  commands; Tab or click completes the highlighted command. Supported commands
  are `/help`, `/init`, `/session`, `/new`, `/clear`, `/resume`, `/model`,
  `/rename <name>`, `/compact [focus]`, `/reflect [focus]`,
  `/skill generated`, `/skill revert <name>`, `/sandbox`, `/sandbox reload`,
  `/approve <id>`, `/deny <id>`, `/tasks`, `/task <id|name>`,
  `/task cancel <id|name>`,
  `/mcp`, and `/exit`. Commands backed by existing server APIs run locally
  instead of starting a provider turn. `/init` submits the same built-in
  `AGENTS.md` contributor-guide task as the terminal frontends. `/resume` asks
  you to choose a session from the sidebar; `/exit` asks you to close the
  browser tab because a page cannot reliably close a tab it did not open.
  `/approve <id>` and `/deny <id>` decide an elevated Bash command the
  running turn waits for, through
  `POST /v1/sessions/{id}/approvals/{approval_id}`; the turn continues with
  the decision. Both run immediately while a turn runs. The transcript shows
  each request with its id and each decision, including decisions made by
  another client.
  `/mcp` shows each configured server's connection state only; signing in
  runs on the host with `otto mcp login <server>`, since the OAuth flow opens
  a browser there, not in the page.
- Enter sends the composer text as a turn or Web command; Shift+Enter inserts
  a newline. Assistant text renders as GitHub-Flavored Markdown, with KaTeX
  math for `$...$` and `$$...$$`, plus Mermaid diagrams in fenced
  `mermaid` code blocks. Each tool call is a collapsible block with its
  arguments and result.
- **Image** selects one PNG, JPEG, or WebP image; pasting a screenshot selects
  it too. The composer shows a preview, and the image can be sent without text.
  Sending stores the original image with the prompt in session history and
  shows it in the transcript, including after resume. The provider receives it
  with `detail: high`.
- While a turn runs, Enter holds the composer text as the next input and
  sends it when the turn ends; Enter again replaces it, and Ctrl+U or
  **Withdraw** removes it. **Cancel turn** calls
  `POST /v1/sessions/{id}/turns/{turn_id}/cancel`.
- The page starts turns with `"queue": true`. When another client's turn
  starts first, the footer shows `queued` until this turn starts, and the
  transcript shows the prompt when it starts.
- Reloading the page during a turn re-attaches to the running turn's event
  stream and continues rendering it; if the stream drops, the page re-reads
  it from the last sequence number it saw.
- While a session is open and idle, the page polls `GET /v1/sessions/{id}`
  about once a second. A server-started wake (`trigger` `task`, such as a
  `remind` timer) is attached if it is still running; if it
  already finished, the page reloads history.

- **Compact** calls `POST /v1/sessions/{id}/compact`; any text in the
  composer is sent as the `focus`. The result appears as a notice in the
  transcript (`Nothing to compact` when the server reports a no-op).
- `/reflect [focus]` calls `POST /v1/sessions/{id}/reflect` and shows the one-line
  report as a notice; the composer is held while it runs, as for a compaction.
  `/skill generated` lists the skills reflection wrote
  (`GET /v1/sessions/{id}/reflection/skills`) and `/skill revert <name>` undoes
  one. Lines that background reflection queues after a compaction appear in the
  transcript within a couple of seconds (the page polls
  `GET /v1/sessions/{id}/notices`); lines queued before the page opened are not
  replayed. The Web UI has no memory review: candidates queued from the browser
  are reviewed in a terminal with `/memory review`.
- A **Tasks** panel appears above the composer when the session has sub-agent
  tasks (`GET /v1/sessions/{id}/tasks`). It re-reads on `notification` events
  and at turn end, polls every 3 seconds while a task is queued or running,
  and offers **Cancel** for those. The model-facing `agent_send` tool can send
  task updates to a queued or running child from a parent turn; child agents can
  use `agent_report` to send the parent an answer to a question or a blocker
  needing its decision, limited to 10 calls on the child's own initiative per
  task plus one more per `agent_send` message received. The Web UI has no
  separate task-send button.
- **Context** in the session bar opens a side panel with the same report as
  the TUI `/context` modal (`GET /v1/sessions/{id}/context`): one bar per
  section, and each section and item expands to show its text. It re-reads
  when opened and at turn end.
- The footer shows `GET /v1/info` (provider, model, sandbox), the session's
  context size and cumulative usage from `GET /v1/sessions/{id}`, and persisted
  all-session token totals and cache hit rate from `GET /v1/usage`; during a
  turn it also totals that turn's `provider_usage` events and shows the same
  phase status line as the TUI. Reasoning summaries render as a collapsed
  block whose first line is the summary.
- The top bar switches between **Chat**, **Usage**, **Workflows**, and
  **Agents**. Usage
  shows persisted totals, a Mermaid token-volume chart for the last 7, 30, or
  90 UTC days, and an exact daily table. It reads `GET /v1/usage/daily` and
  does not expose the SQLite database to the browser.
- **Workflows** lists this workspace's durable runs (`GET /v1/workflows`) and
  starts one from a name and input. Selecting a run shows its steps and
  approval requests and offers **Resume**, **Cancel**, per-step **Retry** and
  **Fork**, and **Approve**/**Reject** for a pending gate, over the
  `/v1/workflows` routes below. See [Durable workflows](#durable-workflows).
- **Agents** lists sub-agent tasks from every session of every Otto process
  (`GET /v1/tasks`), newest first, with status, agent, description,
  workspace, parent session, created time, duration, steps, tool calls, and
  tokens. The status and workspace filters map to the query parameters, and
  **Load more** fetches older rows. It polls every 3 seconds while a listed
  task is queued or running. Selecting a row shows the prompt, the result or
  error, and the child transcript; **Cancel** appears for a queued or running
  task whose parent session this server has open, and the parent session
  opens in Chat when this server lists it.

### HTTP API

API endpoints are under `/v1/`; `/`, `/assets/`, `/healthz`, and `/metrics`
are served at the root. Request and error bodies are JSON.

| Method and path | Behavior |
| --- | --- |
| `GET /v1/workspaces` | List loaded workspaces, startup first: `{"startup", "roots", "workspaces": [{"path", "open_sessions", "workflows"}...]}`. |
| `POST /v1/workspaces` | Admit and load a workspace (`{"path":"...","trust":false}`). `201` when newly loaded, `200` when already loaded. `400 INVALID_WORKSPACE` or `403 WORKSPACE_NOT_ADMITTED` otherwise; with `"trust": true` a not-admitted directory is recorded as trusted in the config file, then loaded. Returns one `workspaces` entry. |
| `GET /v1/fs/dirs?path=...` | List subdirectories for the folder picker: `{"path", "parent", "roots", "dirs": [{"name", "path"}...]}`. Limited to the home directory and `workspace_roots`; `400 INVALID_PATH`, `403 PATH_NOT_ALLOWED`. |
| `DELETE /v1/workspaces?path=...` | Unload a workspace and remove it from `~/.otto/serve-workspaces.json`. `204` on success; `404 WORKSPACE_NOT_FOUND`, `409 WORKSPACE_IS_STARTUP`, or `409 WORKSPACE_IN_USE` otherwise. |
| `GET /v1/workspaces/diff?workspace=<path>` | Read-only changes of a working directory (default the startup workspace) against `HEAD`, or the empty tree before the first commit: staged, unstaged, and untracked files under that directory, ignored files excluded. `{"workspace", "repository", "branch", "files": [{"path", "old_path", "status", "binary", "patch", "truncated"}...], "truncated"}`. `status` is `modified`, `added`, `deleted`, `renamed`, or `untracked`; paths are relative to the directory. `repository:false` when the directory is not in a git work tree. git runs through the workspace's sandbox with external diff and textconv drivers disabled. Limits: 256 KiB of patch per file, about 1 MiB in total, patches for the first 200 untracked files, 10 s for all git commands. `400`/`403` as `POST /v1/workspaces`; `501 diff_unavailable` without a usable sandbox; `500 git_failed`; `504 git_timeout`. |
| `POST /v1/sessions` | Create a session (`{}`, optionally `"workspace":"<path>"`, default the startup workspace) or open one by id (`{"resume":"<id>"}`, 32 lowercase hexadecimal characters, looked up in `workspace` if given, else every loaded workspace, including sessions older than the newest 20 listed). `201` for a new session, `200` for an existing one, including one already open in this process. Returns the session object. |
| `GET /v1/status` | `text/event-stream` of `event: status` snapshots of every session open in this process, in every loaded workspace: `{"sessions":[{"id","workspace","turn","turn_id","queued","approvals","tasks"}...]}`, sorted by workspace, then id. `turn` is `running`, the last finished turn's `ok`, `error`, or `canceled`, or `null` before the first turn; `turn_id` is the running turn's id, else the newest turn's; `queued` counts queued turns; `approvals` counts Bash approvals waiting for a decision; `tasks` counts queued or running sub-agent tasks. The current snapshot is sent on connect and again whenever it changes; there is no replay. The stream ends when the server shuts down. |
| `GET /v1/sessions?workspace=<path>` | List sessions: on-disk sessions merged with sessions currently open in this process, each flagged `open`, with `last_user_text` (at most 80 characters) and `modified` (RFC 3339). Without `workspace`, every loaded workspace; with it, that workspace only. |
| `GET /v1/sessions/{id}` | Return one open session's info. `404` if the session is not open. |
| `PATCH /v1/sessions/{id}` | Rename an open session with `{"name":"dev"}`. `409 turn_active` while a turn is running. |
| `DELETE /v1/sessions/{id}` | Cancel any active turn, close the session, `204`. |
| `GET /v1/sessions/{id}/history?before_turn=<turn_id>` | Return the session's message history. With `before_turn`, return only the messages that existed when that turn started, so a client can replay a running turn's events from sequence `0` without showing its prompt and finished steps twice; `404` when the turn is not retained or has not started. |
| `POST /v1/sessions/{id}/approvals/{approval_id}` | Decide an elevated Bash command a running turn waits for: `{"decision":"allow"}` or `{"decision":"deny"}`. `200 {"decision"}` for the first decision; `409 approval_decided` when it was already decided; `409 approval_failed` when the id is not waiting. See [Approvals inside a turn](#approvals-inside-a-turn). |
| `POST /v1/sessions/{id}/turns` | Start a turn: `{"text":"...","stream":true}`. An optional `image` carries base64 `data` and `mime_type` (`image/png`, `image/jpeg`, or `image/webp`); `text` may be empty when `image` is present. `"queue": true` queues the turn while another runs (see [Turn queue](#turn-queue)). The response header `Otto-Turn-Id` names the turn. `stream` defaults to `true` and returns a `text/event-stream` response starting at sequence `0`; `stream:false` waits for the turn to finish and returns its summary instead. `409 turn_active` while a turn runs and `queue` is not set; `409 queue_full` with 16 turns queued. |
| `GET /v1/sessions/{id}/turns/{turn_id}` | Return a turn summary. The session's queued turns and its most recent started turn are retained. |
| `GET /v1/sessions/{id}/turns/{turn_id}/events?after=N` | Re-read a retained turn's event stream from sequence `N+1`; also honors the `Last-Event-ID` header. A queued turn's stream sends nothing until it starts. |
| `POST /v1/sessions/{id}/turns/{turn_id}/cancel` | Cancel the turn, `202`. A queued turn is removed from the queue. |
| `POST /v1/sessions/{id}/reflect` | Run one reflection now, optionally with `{"focus":"..."}`, and return what it did (`status`, `candidates`, `skills`, `dropped`, and the one-line summary). Memory proposals are queued as pending candidates; skills that pass every check are written to `~/.otto/skills`. `409 turn_active` while a turn runs or is queued, or a compaction or reflection runs; `409 reflection_unavailable` when reflection is disabled, the session has no file, memory is unavailable, or the redaction boundary is closed; `409 reflection_failed` when the model call or its answer failed (nothing is marked covered). |
| `GET /v1/sessions/{id}/reflection/skills` | List the skills reflection wrote and not reverted, with the run, session, time, reason, and `owned` (false once the file was edited by hand). `enabled:false` when reflection is turned off. |
| `POST /v1/sessions/{id}/reflection/skills/{name}/revert` | Restore the previous version of a skill reflection wrote, or remove it if reflection created it (`result` is `restored` or `removed`). `404` when reflection did not write it; `409 skill_not_owned` when it was edited by hand; `409 skills_unavailable` when `[skills]` is disabled. These act on the user's `~/.otto/skills`, not on the session, and are as exposed as the skill enable and disable routes: any holder of the token or socket may call them. |
| `GET /v1/sessions/{id}/notices` | The lines background reflection queued for this session, `{"notices":[{"id":1,"text":"..."}],"last":1}`; pass `?after=<last>` to read only newer ones. They are not removed by reading, so every client sees each one, and only the 20 newest are kept. |
| `POST /v1/sessions/{id}/compact` | Run one context compaction now, optionally with `{"focus":"..."}`, and return the compaction result (`noop:true` when there was nothing to compact). `409 turn_active` while a turn runs or is queued, or another compaction runs; `409 compaction_failed` when the compaction fails and the previous context stays in effect. Closing the request cancels the compaction. |
| `GET /v1/sessions/{id}/context` | Return what the next provider request contains: model, context window, compaction threshold, estimated and last reported input tokens, and sections of items with estimated tokens and text. `409 context_unavailable` when the redaction boundary is closed. |
| `GET /v1/sessions/{id}/tasks` | List the session's sub-agent tasks in creation order. |
| `GET /v1/sessions/{id}/tasks/{task_id}` | Return one task plus its child session's history. |
| `POST /v1/sessions/{id}/tasks/{task_id}/cancel` | Cancel a running task and return it. `409 task_done` if it already finished. |
| `GET /v1/tasks?status=&workspace=&limit=&before=` | List recorded sub-agent tasks from `~/.otto/tasks.db`, from any session and any Otto process, newest first by `created_at`. `status` is `queued`, `running`, `succeeded`, `failed`, `canceled`, or `interrupted`; `workspace` is an exact path; `limit` defaults to 100 and is capped at 500; `before` is the `next_before` cursor of the previous page (empty when there are no older rows). Each task carries `cancelable: true` only when this server has its parent session open and the task is queued or running. |
| `GET /v1/tasks/{parent_session}/{task_id}` | Return one recorded task under `task`, its child transcript as `history`, and `transcript_missing: true` (with an empty `history`) when the transcript file is absent or unreadable. `404` for an unknown task. |
| `GET /v1/sessions/{id}/timers` | List the session's outstanding timers, soonest first: `id`, `fire_at`, and `message`. |
| `POST /v1/sessions/{id}/timers/{timer_id}/cancel` | Cancel one outstanding timer and return it. `404` if no timer has that id. |
| `GET /v1/sessions/{id}/mcp` | List the session's MCP servers and their connection state, in configuration order. |
| `GET/POST /v1/workflows?workspace=<path>` | List that workspace's workflow runs (default the startup workspace), or start one there with `{"name":"...","input":"..."}`. |
| `GET /v1/workflows/{id}` | Return a run, its steps, and its approval requests. Searches every loaded workspace for the run id. |
| `GET /v1/workflows/{id}/events?after=N` | Replay durable workflow SSE events after sequence `N`; unlike turn events, replay survives process restart. |
| `POST /v1/workflows/{id}/resume` | Resume a run; `{"retry":"step-id"}` explicitly retries one interrupted step. |
| `POST /v1/workflows/{id}/fork` | Create a new run from a committed step boundary with `{"after_step":"step-id"}`. |
| `POST /v1/workflows/{id}/cancel` | Cancel a run after its active attempts stop. |
| `POST /v1/workflows/requests/{id}/approve` | Approve a durable workflow gate. Searches every loaded workspace for the request id. |
| `POST /v1/workflows/requests/{id}/reject` | Reject a durable workflow gate and cancel the run. Searches every loaded workspace for the request id. |
| `POST /v1/sandbox/reload` | Re-read `[sandbox]` and apply it to every loaded workspace; `409` while any session in any loaded workspace has a turn in flight, or `409 sandbox_reload_failed` when any workspace's reload fails; `501` when the process has no reloadable sandbox. With one loaded workspace, returns the sandbox object now in effect, as before; with more than one, the startup workspace's sandbox fields stay at the top level and an added `workspaces` array carries each workspace's own `{"workspace", "sandbox", "error"}` (`sandbox` and `error` are mutually exclusive). |
| `GET /v1/info` | Process-level static info: workspace, provider, profile, model, sandbox summary, and the configured profile names. |
| `GET /v1/usage?session_id=<id>` | Aggregate persisted provider token usage across all sessions, or one session when `session_id` is set. |
| `GET /v1/usage/daily?days=30&session_id=<id>` | Return zero-filled daily usage and range totals for 1 to 365 UTC days. `session_id` is optional. |
| `GET /v1/openapi.yaml` | The OpenAPI 3.1 document for this API. |
| `GET /healthz` | `{"status":"ok","sessions_open":N}`. |
| `GET /metrics` | Prometheus text-format metrics. |

A turn keeps running after its client disconnects; `POST .../cancel` is the
only way to stop it.

### Session object

```json
{
  "id": "...",
  "name": "dev",
  "workspace": "...",
  "provider": "...",
  "profile": "...",
  "model": "...",
  "context_window": 0,
  "usage": {
    "input_tokens": 0,
    "output_tokens": 0,
    "cached_input_tokens": 0
  },
  "context_input_tokens": 0,
  "sandbox": {
    "mode": "...",
    "network": "...",
    "bash_available": true,
    "summary": "..."
  },
  "turn": { "id": "...", "trigger": "user", "status": "..." }
}
```

`turn` is `null` when no turn has run yet on this session. `trigger` is
`user` for a turn started by `POST .../turns` and `task` for one the server
started to deliver a finished sub-agent task. The session object
never includes the session's file path; the session ID is the handle used by
every endpoint.

### Turn summary

```json
{
  "id": "...",
  "trigger": "user",
  "status": "ok",
  "error": "",
  "text": "...",
  "usage": { "input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0 },
  "usage_present": true,
  "started_at": "...",
  "finished_at": "..."
}
```

`status` is one of `queued`, `running`, `ok`, `error`, or `canceled`. `error` is omitted
unless `status` is `error`; `finished_at` is omitted while the turn runs.

### Events

Each SSE frame carries `id: <sequence>`, `event: <name>`, and a JSON `data:`
payload. Event names are the `agent.Event` type names: `agent_started`,
`text_delta`, `reasoning_delta`, `tool_call_started`, `tool_call_finished`, `provider_usage`,
`provider_retry`, `compaction_planned`, `compaction_started`, `compaction_completed`,
`compaction_warning`, `memory_warning`, `agent_finished`, and `agent_error`,
plus `notification` when a sub-agent task finishes. `reasoning_delta` carries
`text` like `text_delta`. `provider_retry` carries
`retry: {attempt, max_attempts, delay_ms, reason}`, where `attempt` is the
1-based attempt about to start and `reason` is an HTTP status such as
`HTTP 503`, `connection error`, or `stream interrupted`; it never contains the
response body.

### Errors

An error body has the shape `{"error":{"code":"...","message":"..."}}`.
Status codes:

| Status | Meaning |
| --- | --- |
| `400` | Empty or missing turn text, or an invalid JSON body. |
| `401` | Missing or invalid bearer token (TCP listener only). |
| `404` | Session, turn, or task not found. |
| `409` | `turn_active`: a turn is running or queued, or a compaction is active, on this session. `queue_full`: 16 turns are already queued. `approval_decided` and `approval_failed` name the approval conflicts. `compaction_failed`, `reflection_unavailable`, `reflection_failed`, `skill_not_owned`, `skills_unavailable`, `revert_failed`, `sandbox_reload_failed`, and `task_done` name the other conflicts. |
| `500` | Internal error. The response body is a fixed `internal error` message; details go to the server log only. |

### Observability

Each normal provider response, compaction summary, and sub-agent provider
response appends a content-free row to `~/.otto/usage.db`. The row contains
time, workspace/session/task identifiers, provider/profile/model, usage
presence, and token counts; it never contains prompts, response text, tool
arguments, or tool output. Collection, SQLite storage, and the HTTP/UI query
path are separate boundaries.

`GET /v1/usage` returns all recorded totals; pass `session_id` to restrict the
query. `cache_hit_rate` is the weighted ratio
`cached_input_tokens / input_tokens`. A provider that omits its cache-token
breakdown contributes zero cached tokens.

`GET /v1/usage/daily` returns the same metrics grouped by UTC date and fills
days without provider calls with zeroes. The Web UI uses the existing
MIT-licensed Mermaid dependency for its chart; no separate analytics or chart
backend is involved.

`GET /metrics` exposes `otto_http_requests_total{route,method,status}`,
`otto_http_request_duration_seconds{route}`,
`otto_provider_api_requests_total{provider,model,status}`,
`otto_provider_api_request_duration_seconds{provider,model}`,
`otto_sessions_open`, `otto_session_context_window_tokens{session_id,provider,model}`,
`otto_session_context_input_tokens{session_id,provider,model}`,
`otto_session_context_input_tokens_pending{session_id,provider,model}`,
`otto_turns_total{status}`, `otto_turns_active`, `otto_turn_duration_seconds`,
`otto_tool_calls_total{tool,status}`, `otto_tool_call_duration_seconds{tool}`,
`otto_provider_tokens_total{kind}`, `otto_event_stream_clients`,
`otto_tasks_started_total`, `otto_tasks_finished_total{status}`,
`otto_tasks_running`, `otto_workflow_runs{status}`, and
`otto_workflow_steps{status}`.

Otto logs one line per HTTP request (method, route, status, duration, request
ID) and one line per turn start and finish (session ID, turn ID, status,
duration, token usage). Prompt text and tool arguments or output are never
logged.

### Sub-agent task records

Every sub-agent task started by the `agent` tool is recorded in
`~/.otto/tasks.db` (SQLite, file mode `0600`), one row per task, keyed by
parent session and task id. Otto writes the row when the task is queued and
again when it starts, after each provider step and tool call, and when it
finishes. Each Otto process writes only its own tasks; `/agents`, the Web
UI's **Agents** view, and `GET /v1/tasks` read every process's rows.

A row holds the workspace, the parent session id and file (for a
`--no-session` parent, the id is `memory:<pid>:<process start time>` and the
file is empty), the agent, name, description, model, and context mode, the
status and its timestamps, step and tool-call counts and the last tool,
input, output, and cached tokens, the child transcript path, the owning
process id and start time, and the first 64 KiB each of the prompt, the
result, and the error. These texts are stored in plaintext, like the session
files that already hold them in full. Nothing is sent off the machine, and
rows are never pruned.

A queued or running row whose owning process has exited (or whose process id
now belongs to a different process) is shown as `interrupted`; the stored row
is not changed.

If `tasks.db` cannot be opened, or was written by a newer Otto with a schema
this build does not know, Otto prints one warning at startup and runs without
recording. A write error later in the process stops recording for that
process without output. Neither affects the task.

`agent_send` messages and `agent_report` calls are runtime delivery only: they
are not rows in `tasks.db`. If the child receives an `agent_send` message, it
is written into that child's transcript as parent-message context when the
child reaches the next normal notification checkpoint.

### Shutdown

`otto serve` shuts down on `SIGINT` or `SIGTERM`: it stops accepting new
requests, cancels every active turn and compaction, closes every session,
removes the socket file (socket mode), and exits `0`. With
`--exit-on-stdin-close`, end of file on stdin, or a failed read, starts the
same shutdown. When an open session holds a lease, `SIGTERM` first moves
every open session as described in
[Continuing a session on another host](#continuing-a-session-on-another-host),
and a second `SIGTERM` exits at once; without a lease a second `SIGTERM` has
no effect.

### Examples

```bash
curl -s --unix-socket ~/.otto/otto.sock -X POST http://otto/v1/sessions -d '{}'
curl -N --unix-socket ~/.otto/otto.sock -X POST http://otto/v1/sessions/<id>/turns -d '{"text":"list files"}'
curl -s --unix-socket ~/.otto/otto.sock -X POST http://otto/v1/sessions/<id>/turns/<turn_id>/cancel

# TCP listener; TOKEN is the value printed at startup
curl -s -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8787/v1/sessions -d '{}'
curl -s -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8787/v1/sessions/<id>/compact -d '{}'
```

### Not supported

- Non-loopback TCP binds, TLS, and CORS; the TCP listener is for clients on
  the same machine.
- Authentication on the Unix socket beyond file permissions (owner-only
  directory and socket modes); there is no peer-uid check. Token persistence
  or rotation on the TCP listener.
- Streaming compaction progress; `POST .../compact` returns only the result.
- Event replay across turns; only the most recent turn per session is readable.
  Workflow events are separate and durable.
- Idle session eviction or a limit on how many sessions can stay open.
- SSE heartbeats.

### Desktop app (macOS)

The desktop app (`desktop/`) is a native macOS window around `otto serve`. On
launch it captures the login shell's environment, asks for a folder the
first time it runs (or when the saved folder no longer exists), runs `otto
trust` on it, starts `otto serve --exit-on-stdin-close` as a child process,
and loads the child's HTTP address in its main window once the child
announces it. The window loads only that address. The page is a client of
the same HTTP API described in this section. The sidebar's **Add
workspace…** button runs the same action as **File > Open Folder…**.

**File > Open Folder…** (`⌘O`) picks another directory, runs `otto trust` on
it, registers it with the running server over `POST /v1/workspaces`, and
reloads the window. Quitting the app sends the child `SIGTERM`, then
`SIGKILL` after 10 seconds if it has not exited.

State (the last opened folder) is stored at `~/Library/Application
Support/com.otto.desktop/state.json`. The child's stderr is appended to
`~/Library/Logs/com.otto.desktop/serve.log`; if the child exits before
announcing its address, or does not announce one within 30 seconds, the app
shows the exit status and the last 50 lines of that log.

Building, checking, and releasing the app are covered in
[`desktop/README.md`](../desktop/README.md).

## ACP agent server

`otto acp` runs Otto as an [Agent Client Protocol](https://agentclientprotocol.com)
(ACP) v1 agent: newline-delimited JSON-RPC 2.0 on stdin and stdout, started by
an ACP client as a child process. stdout carries only JSON-RPC frames;
warnings and diagnostics go to stderr.

```bash
otto acp [--config PATH] [--cwd PATH] [--profile NAME] ...
```

`acp` accepts `--config`, `--cwd`, `--profile`, `--provider`, `--base-url`,
`--model`, `--thinking`, `--sandbox`, `--shell-timeout`, and
`--max-output-bytes`. It rejects `--ui`, `--prompt`, `--resume`,
`--continue`, `--no-session`, `--archive`, and the `serve`-only flags;
`--socket` is accepted only with `--attach`.

One process serves one workspace: `--cwd`, else the process working
directory. A request whose `cwd` resolves to another directory is rejected.
The workspace's sandbox and memory are set up at startup as for `otto serve`;
MCP servers start when a session is created or loaded. A process may hold
several sessions. A second prompt in a session that is already running one
is rejected.

### Methods

| Method | Behavior |
| --- | --- |
| `initialize` | Reports protocol version 1, `loadSession`, and `session/list`. Prompts accept text only; images, audio, and embedded context are not accepted. No authentication methods: credentials come from environment variables and `otto login`. |
| `session/new` | Creates a session. No session file is written until the first prompt. |
| `session/load` | Opens a session of this workspace by its 32-character id, sends the stored conversation as `session/update` notifications, then responds. The response also carries `sessionId`. |
| `session/list` | The newest 20 sessions of the workspace, titled by session name or the last user message (truncated to 80 characters). |
| `session/prompt` | Runs one turn. Text blocks are joined with newlines; a `resource_link` block becomes a line `<name>: <uri>`. Returns `end_turn`, or `cancelled` after `session/cancel`. |
| `session/cancel` | Cancels the session's running prompt and any pending permission request. |
| `_otto/memory/pending` | Extension. Params `{sessionId, cursor?, limit?}` (`limit` 1 to 50, default 20); returns `{candidates, nextCursor}` with candidates as `{id, action, kind, key, text, reason, origin, scope}`; `nextCursor` is `""` on the last page. |
| `_otto/memory/review` | Extension. Params `{sessionId, candidateId, decision}` with `decision` `accept` or `reject`; decides one candidate as a human review and returns `{decision, candidateId, record, forgotten}`. Errors: `-32010` memory is not available in the session, `-32011` the candidate was already decided or changed, `-32002` unknown session or candidate, `-32602` bad parameters or cursor. `otto acp` advertises both with `agentCapabilities._meta.otto.memoryReview`; `otto acp --attach` does not serve them. |

`mcpServers` in `session/new` and `session/load` must be empty: MCP servers
come from Otto's own configuration (see [MCP servers](#mcp-servers)). Otto
does not call the client's `fs/*` or `terminal/*` methods; tools run inside
Otto as in the other frontends.

During a prompt Otto sends assistant text as `agent_message_chunk`,
reasoning as `agent_thought_chunk`, and each tool call as a `tool_call`
followed by a `tool_call_update` with status `completed` or `failed`.
`bash` calls have kind `execute` and the command as title.

### Elevated `bash` uses `session/request_permission`

When the model asks for an unsandboxed `bash` command (see
[`bash` sandbox policy](#bash-sandbox-policy)), the turn finishes as in the
other frontends. Otto then sends `session/request_permission` with the
command and two options, **Allow once** and **Deny**. Allow once grants that
command and runs the retry turn inside the same `session/prompt`. Deny, a
cancelled request, or `session/cancel` ends the prompt; the approval stays
pending as when a TUI user does not enter `/approve`. There is no "always"
option over ACP.

### Background results arrive with the next prompt

`otto acp` does not start turns on its own. A finished sub-agent, a timer,
or another inbox item reaches the model before the next prompt's text.

### Shutdown

End of file on stdin, `SIGINT`, or `SIGTERM` cancels every running prompt,
answers each with `cancelled`, closes every session, and exits `0`. A
`SIGTERM` while a session holds a lease first moves it, as for `otto serve`
(see [Continuing a session on another host](#continuing-a-session-on-another-host)).
A client that kills the process with `SIGKILL` loses the assistant message
being streamed; everything Otto had already written to the session file
remains.

### `--attach` forwards requests to `otto serve`

```bash
otto acp --attach [--socket PATH]
```

`otto acp --attach` holds no session: it forwards each ACP method to a
running `otto serve` over its Unix socket, so the session it uses can be open
in the web UI, in another `otto acp --attach` process, and in `otto serve`'s
other clients at the same time. It runs no model or tool and reads no
provider credentials. The socket defaults to `[server].socket`, then
`~/.otto/otto.sock`; TCP is not supported. The `initialize` response, the
workspace rule and the `cwd` checks are those of `otto acp`, and `otto serve`
must admit the workspace (see [Workspaces](#workspaces)).

At startup it requests `GET /healthz`. If `otto serve` does not answer, it
prints `otto serve is not reachable at <path>: <error>` to stderr and exits
with status `1`.

| Method | Request to `otto serve` |
| --- | --- |
| `session/new` | `POST /v1/sessions` with the workspace. |
| `session/load` | `POST /v1/sessions` with `resume`, then `GET .../history`, sent as `session/update` notifications before the response. Any session id of the workspace can be loaded, including one older than the newest 20. |
| `session/list` | `GET /v1/sessions?workspace=...`. |
| `session/prompt` | `POST .../turns` with `"queue": true`. A prompt sent while the session runs another client's turn waits in `otto serve`'s queue (see [Turn queue](#turn-queue)). |
| `session/cancel` | Cancels the prompt's turn, queued or running. |

- Only turns started by this process are forwarded; turns from other
  clients of the same session are not sent as `session/update`.
- A turn that ends with an error answers the prompt with a JSON-RPC error
  carrying the redacted message.
- An elevated `bash` command sends `session/request_permission` while the
  turn waits (see [Approvals inside a turn](#approvals-inside-a-turn)).
  **Allow once** allows it; any other answer denies it. If another client
  decides first, the request is withdrawn with `$/cancel_request` and the
  turn continues with that decision.
- If the connection to `otto serve` fails, or a turn's event stream ends
  without its `turn_end` frame, every running prompt is answered with the
  JSON-RPC error `connection to otto serve lost` and the process exits with
  status `1`.

### Chat access goes through otto-connect

To use Otto from Telegram, run `otto-connect`, which starts `otto acp` as
its ACP agent. See [Chat connector](#chat-connector).

### Not supported over ACP

- `session/resume`, `session/close`, `session/delete`, session modes and
  config options, and client-supplied MCP servers.
- Image, audio, and embedded-resource prompt content.
- Slash commands such as `/approve` or `/sandbox`: text from the client
  reaches the model as a user message.

## Chat connector

`otto-connect` connects Telegram and Feishu (Lark) chats to `otto acp`. It is
a separate Go program in `connect/`: it starts one `otto acp` process as a
child, acts as its ACP client, and maps each chat to one Otto session.
Telegram and Feishu can be enabled in the same process.

### Building and running

Building requires Go at the version in `connect/go.mod`.

```bash
make connect-build                 # writes target/otto-connect
export OTTO_CONNECT_TELEGRAM_TOKEN=<bot token from @BotFather>   # for [telegram]
export OTTO_CONNECT_FEISHU_APP_SECRET=<app secret>               # for [feishu]
target/otto-connect [--config PATH]
```

`otto-connect` runs in the foreground and logs to stderr. `SIGINT` or
`SIGTERM` sends `session/cancel` for every running prompt, stops polling
Telegram and closes the Feishu connection, closes the agent's stdin, and
kills the agent if it has not exited 10 s later. A config error, a token that
Telegram rejects (HTTP 401 or 404 from `getMe`), or an app id or secret that
Feishu rejects exits with status 1 and `otto-connect: <message>`. A Feishu
connection that fails for another reason, such as no network, is retried by
the SDK and each failure is logged as `feishu connection error`.

### Configuration

`~/.config/otto/connect.toml`, or the file given with `--config`:

```toml
[agent]
command = ["otto", "acp"]          # the default; resolved through PATH
workspace = "/Users/me/work"       # required, absolute

[telegram]
token_env = "OTTO_CONNECT_TELEGRAM_TOKEN"
chats = ["123456789"]
senders = ["123456789"]

[feishu]
app_id = "cli_xxx"
app_secret_env = "OTTO_CONNECT_FEISHU_APP_SECRET"
domain = "feishu"                  # the default; "lark" for Lark (larksuite.com)
chats = ["oc_xxx"]
senders = ["ou_xxx"]
```

- A platform is enabled when its section is present. At least one of
  `[telegram]` and `[feishu]` is required.
- The agent process starts with `workspace` as its working directory, and
  every session uses it as `cwd`. Otto flags go into `command`, for example
  `["otto", "acp", "--profile", "work"]`.
- The Telegram bot token is read from the environment variable that
  `token_env` names, and the Feishu app secret from the one that
  `app_secret_env` names; an empty or unset variable fails startup. A `token`
  or `app_secret` key in the file, like any unknown key, fails the config
  load. `app_id` is not a secret and is required. The agent process is
  started without these variables, so tools run by Otto cannot read them.
- `otto acp` inherits the rest of the environment and reads its provider key
  as usual (the profile's `api_key_env` variable or `OTTO_API_KEY`), or uses
  the `chatgpt` provider after `otto login`.
- State is kept in `~/.otto/connect/state.json` (mode `0600`): the session id
  of each chat and the Telegram update offset.
- Log lines contain chat ids, sender ids, and errors. Message text is not
  logged.

### Feishu app

In the Feishu (or Lark) developer console:

1. Create a self-built app and enable the bot capability.
2. Grant the message permissions, at least `im:message` and
   `im:message:send_as_bot`.
3. Under events and callbacks, select long connection mode and subscribe to
   `im.message.receive_v1`, and enable the `card.action.trigger` card callback.
4. Publish the app version; publish again after changing permissions or
   events.
5. Add the bot to a group, or open a private chat with it.

`otto-connect` opens the long connection itself, so no public address or
`lark-cli` is needed.

### Sharing sessions with `otto serve`

With `command = ["otto", "acp", "--attach"]`, the agent process is the
relay described in [`--attach` forwards requests to `otto serve`](#--attach-forwards-requests-to-otto-serve):
sessions, models, and tools run in the `otto serve` on the socket, and the
web UI of that serve shows the same sessions.

```toml
[agent]
command = ["otto", "acp", "--attach"]   # add "--socket", "/path" for a non-default socket
workspace = "/Users/me/work"            # the same workspace the serve clients use
```

- Start `otto serve` before `otto-connect`. The relay reads no provider
  credentials; the serve process needs them.
- A chat's message and a web UI message on the same session wait in serve's
  turn queue and run one after the other. The chat gets the reply of its own
  turns only; turns started in the web UI are not sent to the chat.
- Use `/sessions` and `/use` (see [Commands](#commands)) to bind a chat to a
  session that was started in another client.

### Admission

A message is handled only when its chat id is in `chats` and its sender id
is in `senders`. An empty list admits nothing, and startup logs a warning
that the platform will ignore all messages. A rejected message gets no reply
and is logged with its chat and sender ids; send the bot a message and read
that log line to find the ids to add. In a Telegram private chat the chat
id equals the user id; Telegram group ids are negative numbers.

In a Telegram group, a message must also mention the bot (`@BotName`), reply
to one of the bot's messages, or be a command addressed to it
(`/stop@BotName`). With BotFather's privacy mode on (the default), Telegram
delivers only such messages to the bot.

Feishu chat ids start with `oc_` and sender ids (open ids) with `ou_`. A
private chat with the bot has its own `oc_` chat id, which must be in
`chats` as well. In a Feishu group, a message must mention the bot; the
mention is removed from the prompt, so `@Bot /stop` is the `/stop` command.
The Feishu SDK applies the same lists before `otto-connect` sees a message;
the messages it rejects are logged with the same `message rejected` line and
a `reason`. A group message that does not mention the bot is ignored without
a log line.

### Messages and replies

- Each chat has one Otto session. The first message creates it; after a
  restart of `otto-connect` or of the agent, the next message loads it.
  The replayed history of a loaded session is not sent to the chat.
- If the session cannot be loaded (for example, without `--attach`, it is
  open in the TUI or in `otto serve`), the chat gets "The previous session
  could not be loaded; started a new one." and the message runs in a new
  session.
- One prompt runs per chat at a time; further messages wait in a queue of at
  most 10. A message beyond that gets "Queue is full (10 messages); the
  message was not queued." Chats on different sessions run concurrently.
  Chats bound to the same session (with `/use`) take turns: one prompt runs
  on a session at a time, and when three or more chats wait for the same
  session, the order in which they run is not guaranteed.
- The reply is sent when the turn ends, as a reply to the message that
  started it. It contains the assistant text only: reasoning and tool calls
  are not sent, and text before and after a tool call is separated by a
  blank line. Telegram replies are plain text, split at line boundaries into
  parts of at most 4096 characters, and the chat shows "typing" while the
  turn runs. Feishu replies are Markdown, split by the Feishu SDK into parts
  of at most 3500 characters with code blocks kept closed; Feishu shows no
  typing indicator.
- Only text is sent to Otto. A photo, file, or other attachment gets
  "Attachments are not supported and were ignored."; a Telegram caption is
  sent as the prompt. In Feishu, images, files, audio, video and stickers get
  the same notice; the text of a rich-text message is sent and its images are
  not.
- A message received by the platform more than 30 minutes before
  `otto-connect` reads it is not run; the chat gets a notice with the
  message's time.
- Each Telegram message is acknowledged once it is queued or rejected; each
  Feishu message is acknowledged when the SDK receives it, before it is
  queued. A crash therefore does not run a message twice; a message
  acknowledged but not yet run when `otto-connect` stops is not run.
- Feishu messages that arrive close together are delivered one by one, each
  with its own sender and reply target; the SDK's merging of messages is
  turned off.

### Commands

A message whose whole text is one of these commands (for `/use`, the
command and its argument) is handled by `otto-connect` and not sent to Otto.
Any other text, including other words starting with `/`, is a prompt.

| Command | Effect |
| --- | --- |
| `/new` | The chat's next message starts a new session. The old session stays in Otto's session store. |
| `/stop` | Cancels the running turn (reply "Stopped.") and clears the chat's queue. With nothing running: "Nothing is running." |
| `/allow` | Answers the pending permission request with Allow once. |
| `/deny` | Answers the pending permission request with Deny. |
| `/sessions` | Lists the 10 newest sessions of the workspace, one per line: `*` for the chat's session or a space, the first 8 characters of the id, the last change as `YYYY-MM-DD HH:MM` (`-` when unknown), and the title (`(untitled)` when empty). With none: "No sessions." |
| `/use <id>` | Binds the chat to a session and loads it. `<id>` is a full session id, or a prefix of at least 4 characters of a session that `session/list` returns (the newest 20). Reply: "Using session `<id>`: `<title>`". |
| `/memory` | Lists the chat session's pending memory candidates, one per line: the first 8 characters of the id, the action, `kind/key`, the text (200 characters at most), and the origin. With none: "No pending memory candidates." |
| `/memory accept <id>` | Accepts one pending candidate, which writes the record. `<id>` is a full candidate id or a unique prefix of at least 4 characters. |
| `/memory reject <id>` | Rejects one pending candidate; no record is written. |

`/use` replies with one of these when it does not switch:

| Case | Reply |
| --- | --- |
| No argument | "Usage: /use <session id>" |
| Prefix shorter than 4 characters | "A session id prefix needs at least 4 characters." |
| Prefix matches several listed sessions | "\"`<prefix>`\" matches `<n>` sessions; use more characters." |
| Prefix matches no listed session | "No listed session starts with \"`<prefix>`\"." |
| A message of the chat is running or queued | "A message is running or queued; /use is refused. Send /stop first." |
| The agent refuses the load | "Error: could not load session `<id>`: `<error>`" |

A message that arrives while `/use` is loading gets "A session switch is in
progress; the message was not queued." A failed `session/list` call in
`/sessions` or `/use` gets "Error: session/list failed: `<error>`".

### Provider-profile management

When the connector starts `otto acp --attach`, the same admitted chat senders
can manage Otto's existing provider profiles through the running `otto serve`:

```text
/config
/config profiles
/config show <profile>
/models <profile>
/config add <profile> --provider <openai-compatible|chatgpt> --model <model> [--base-url <url>] [--api-key-env <name>] [--thinking <level>]
/config set <profile> <model|thinking|base-url|api-key-env> <value>
/config use <profile>
/config remove <profile>
/config confirm <token>
/config cancel
```

`/config` and `/config profiles` list profile names, providers, models and the
default marker. `/config show` also shows the endpoint and API-key environment
**variable name**, never its value. `/models` asks an OpenAI-compatible
endpoint for its model IDs; ChatGPT profiles report that model enumeration is
not supported rather than guessing an ID.

Every mutation first returns a redacted preview and a one-time confirmation
token. Send `/config confirm <token>` from the same chat and sender within ten
minutes to write exactly that change, or `/config cancel` to discard it. The
connector never accepts API-key values, bot secrets, raw TOML, arbitrary file
writes, or providers other than `openai-compatible` and `chatgpt`.

A saved profile change requires restarting `otto serve` before it becomes
available to new sessions; it never silently changes an existing session. These
commands need attach mode. With direct `otto acp`, the connector replies with
the attach requirement and does not send the command to the model.

Management failures include a `request_id` when a write preview fails. Search
connector and Otto server logs for that ID. Normal logs contain lifecycle IDs,
profile names and outcomes, but never chat text, model text, tool contents,
configuration bytes, API keys, bot tokens, OAuth credentials, or environment
variable values.

### Memory review

`remember` and `forget` only queue candidates; a person decides them. In a
chat, **Approve / Deny** buttons and `/memory accept` / `/memory reject` make
that decision: the connector
sends them to `otto acp` as the ACP extension requests `_otto/memory/pending`
and `_otto/memory/review`, which Otto records as a human review. The model has
no tool for this, and text in the chat such as "approve" is just a prompt.
The commands need the chat to have a session (otherwise "This chat has no
session yet; send a message first."), work while a turn is running, and
reply "This agent does not support memory review." when the agent does not
advertise it, as `otto acp --attach` does not. An id that is not a unique
pending prefix gets "No single pending candidate starts with \"`<id>`\"."; other
arguments get "Usage: /memory | /memory accept <id> | /memory reject <id>".
After each successful turn, Telegram and Feishu show pending memory candidates
as cards with **Approve / Deny** buttons, including create, update, and forget
proposals. `/memory` refreshes the cards; old buttons become inactive. Buttons
are bound to the original chat session and candidate, and use the same chat and
sender allowlists as text commands. A successful decision or a conflict removes
the buttons. Failed card delivery falls back to text commands. After restarting
the connector, use `/memory` to get fresh cards.

`/memory` shows the first page (20) and says when more are pending; an id
is looked up across pages. When a turn called `remember` or `forget`, the
reply ends with "Memory changes are proposals. Send /memory to review them."
The tool results of `remember` and `forget` also tell the model that only the
user decides a candidate and how, so the model can point the user to
`/memory`; it still cannot decide one. Editing a candidate during review is
not available from the chat; use `/memory review` in the REPL or TUI.

The local REPL immediately prints **"Memory review available. Run `/memory
review`."** after any pending proposal is saved. The local TUI opens a review
modal for the current user/workspace scopes: use Up/Down to choose, `a` to
accept, `r` to reject, or Esc to close without changing anything. The modal
reads that scope's existing candidate details for review. The availability
signal itself contains no candidate content and is process-local, so attached
TUI, web, and chat connectors retain their existing refresh/command behavior.

### Permission requests

When Otto asks to run an unsandboxed `bash` command (see
[Elevated `bash` uses `session/request_permission`](#elevated-bash-uses-sessionrequest_permission)),
the chat gets the command with **Approve** and **Deny** buttons (Telegram inline
keyboard or Feishu interactive card). The first button click or `/allow` or
`/deny` from an admitted sender in that chat answers it once. Buttons are bound
to that request; old cards cannot answer a newer request. After a decision,
cancellation, or expiry, the connector replaces the buttons with the final status.
If sending the card fails, it falls back to text commands. `/stop`
cancels it. With no answer after 10 minutes the request is denied and the
chat gets "Permission request timed out; denied." `/allow` or `/deny` with
no pending request gets "No pending request."

With `--attach`, the same request is also shown in the web UI of
`otto serve`. When another client decides it first, serve's own
10-minute timeout denies it, or the turn ends before an answer, the relay
withdraws the request (`$/cancel_request`) and the chat gets "permission
request answered elsewhere". A later `/allow` or `/deny` gets "No pending
request."

### Agent process exits

If `otto acp` exits, each running turn ends and its chat gets one message
with the exit status and the last 20 lines of the agent's stderr. The next
message starts the agent again. After consecutive exits within 60 s of a
start, the restart waits 1 s, then 2 s, 4 s, and so on up to 60 s.

With `--attach`, the relay exits with status 1 when `otto serve` stops or
the socket connection breaks, and a relay started while serve is not
running exits with status 1 and the stderr line "otto serve is not reachable
at `<path>`: `<error>`". Both reach the chat as an agent exit. Restart
`otto serve`; the next message starts the relay again.

### Not supported by otto-connect

- Streaming partial replies; the reply is sent when the turn ends.
- Images, files, and other attachments.
- Running as a login service (launchd or systemd).

## Durable workflows

Durable workflows are explicit DAGs for work whose order, concurrency, human
gates, and restart behavior must not depend on the parent model improvising a
plan. They are separate from the interactive session and continue to be
inspectable after `/new`, `/resume`, or process restart.

Definitions are discovered from `~/.otto/workflows/<name>.toml` and
`<workspace>/.otto/workflows/<name>.toml`; the workspace file wins. Agent and
handoff steps reference named definitions from the existing `AGENT.md` catalog:

```toml
version = 1
description = "Research, review, then ask before delivery."

[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence and report it."

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review the evidence."
needs = ["research"]

[[steps]]
id = "approve"
kind = "approval"
prompt = "Approve delivery?"
needs = ["review"]

[[steps]]
id = "deliver"
agent = "executor"
prompt = "Deliver the approved result."
needs = ["approve"]
```

A handoff step makes a transfer explicit while using the same execution and
recovery behavior as an agent step:

```toml
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over from research and review the result."
needs = ["research"]
```

Root steps run concurrently up to `[agents].max_parallel`; a dependent becomes
ready only after every named predecessor succeeds. Each agent or handoff step
receives the run input and the successful results of its direct predecessors
under fixed headings. There is no template language. Definitions have at most
32 steps, must be acyclic, and are validated completely before the run is
stored.

When a run starts, Otto snapshots the workflow plus each referenced agent's
instructions, model choice, tool allowlist, and write policy. Editing the
source files affects new runs only. The current sandbox remains authoritative
and a tool that no longer exists fails closed.

Agent definitions may declare a workflow write policy in `AGENT.md`
frontmatter:

```markdown
---
name: planner
description: Propose edits without changing files.
tools: read, grep, find, ls
write_policy: propose_only
---
```

```markdown
---
name: executor
description: Apply approved edits under owned paths.
tools: read, grep, find, ls, edit, write, bash
write_policy: owned_paths
write_paths: crates/otto/**, docs/**
---
```

Supported policies are `read_only`, `propose_only`, `single_writer` (the
default), and `owned_paths`. `read_only` and `propose_only` deny workspace
mutation tools even if `tools` lists `edit` or `write`; `owned_paths` allows
`edit` and `write` only for paths matching its comma-separated ownership globs
such as `crates/otto/**` or `docs/*.md`. Workflow validation rejects agent steps
that may run concurrently when their write scopes overlap, unless they are
ordered with `needs` or use disjoint `owned_paths`. This makes the recommended
shape explicit: concurrent planner/reviewer steps produce proposals, an
approval gate records the human decision, and one executor step applies the
approved plan.

State is stored in `~/.otto/workflows.db` with mode `0600`; attempt transcripts
are append-only Pi v3 files under `~/.otto/workflow-sessions`. Only one Otto
process may mutate workflows for one workspace at a time. `otto serve` exposes
the same controller through the Web UI and `/v1/workflows` routes. While the
server owns the workspace lock, use those surfaces rather than a second
`otto workflow` CLI process.

Recovery is deliberately conservative:

- Completed steps stay completed.
- Unstarted ready steps may run after an explicit `resume`.
- Pending approval requests keep the same request ID.
- A step that was running becomes `interrupted` and the run becomes `paused`.
- A configured `workflow_step_timeout` has the same durable result: the active
  attempt and step become `interrupted`, the run becomes `paused`, and no
  dependent step is admitted. Cleanup of the active effectful boundary is
  awaited rather than detached.
- Interrupted steps are never retried automatically. Use
  `otto workflow resume <run-id> --retry <step-id>` only after considering
  whether its last tool call may already have caused an external effect.

Time travel is an explicit fork, not rewind. `otto workflow fork <run-id>
--after-step <step-id>` creates a new run from that step's committed success
event. Steps already succeeded by that event are copied as immutable references
to the source run's attempts; later steps are scheduled normally. The original
run is unchanged, and Otto does not roll back external side effects.

The workflow runtime is fail-fast and supports agent, static handoff, and
boolean approval steps. It does not implement loops, conditions, group chat,
nested workflows, free-form human input, automatic retry, or OpenTelemetry
export.

## Memory

Otto has a local, per-workspace/per-user memory store backed by SQLite/FTS5
(`crates/otto`'s `memory` module). It is enabled by default.

Config (`[memory]` in TOML; all keys optional):

```toml
[memory]
enabled = true
backend = "sqlite"
required = false
recall_tokens = 2000
max_results = 12
require_encryption = false

[memory.sqlite]
path = "~/.otto/memory/memory.db"
busy_timeout = "5s"

[memory.workspace_ids]
"/canonical/path/to/workspace" = "stable-id"
```

There are no `--memory-*` CLI flags.

What's wired:

- Recall before each turn into a request-local, untrusted context block that is
  never written to Pi session JSONL, compaction summaries, or logs.
- Agent tools: `memory_search`, `remember`, `forget`.
- Human commands in both frontends: `/memory list`, `/memory show`, `/memory search`, `/memory forget`,
  `/memory review`, `/memory review <id> accept|reject`, and `/remember`.
- `/reflect [focus]` in the TUI, the REPL, and the Web UI: reflection, described below.
- Standalone CLI: `otto memory status`, `otto memory list`, `otto memory show <id>`, and
  `otto memory forget <id>`.

`/memory list` and `otto memory list` page active records in stable update-time order. They
show the current user and workspace scopes by default; use `--scope user` or `--scope workspace`
to narrow that set, or explicit `--scope all` to inventory every stored workspace. A page prints
`next_cursor=...` when another page exists; pass that value back with `--cursor`. `show` reads one
record from the current scopes (or the selected user/workspace scope).

Model-originated writes always land as pending candidates for human review.
`/memory review` lists the pending candidates for the current user and workspace scopes (id,
scope, action, kind, key, text, reason, confidence, origin), and `/memory review <id> accept`
or `/memory review <id> reject` decides one. Accepting creates the active record (or applies the
update or forget); rejecting leaves memory unchanged.
Human `/remember` and `/memory forget` apply immediately.

### Reflection

`/reflect [focus]` looks back at the part of the current session that no earlier
reflection covered and asks the session's own model, once and with no tools, which
durable facts and preferences are worth remembering, and which procedures are worth
reusing as skills. Each memory survivor is queued as a pending candidate (origin
`extractor`) that you review with `/memory review`; reflection never writes a memory
record itself, and it appends nothing to the session file. Skills are different: a skill
that passes every check below is **written and active without a review step**, as
described under [Generated skills](#generated-skills). `focus` is free text that steers
what to look for.

What reflection checks before queueing a candidate, all in code rather than by asking the
model:

- Every proposal must cite transcript entries with quotes that appear verbatim in them.
  A `preference` must cite one of your own messages. Proposals whose quotes do not match
  are dropped.
- Content from outside you and the workspace is withheld from the model and cannot be
  cited: MCP tool results, chat messages that the removed `[inbound.feishu]` recorded in
  older sessions, and `bash` results whose
  command invokes a network tool (`curl`, `wget`, `ssh`, `git clone|fetch|pull|push`, `gh`,
  a URL, and similar). The check reads the command text, so it narrows the exposure but does
  not see network access hidden inside a script. A prompt that `otto-connect` forwards
  from a chat is a user message, including one from another listed sender in a group.
- Text goes through the same secret redaction as a normal request, and a closed redaction
  boundary stops the run.
- A proposal that repeats a candidate already pending is skipped. The store clears a
  rejected candidate's content, so a rejected proposal can be proposed again by a later run.

Each run is recorded in `~/.otto/reflection.db` (ids, entry ranges, counts, and status; no
transcript text). A completed run advances a per-session watermark, so the next run covers
only newer entries; a failed or canceled run leaves it in place and the next run covers the
same entries again. Provider usage is recorded in the usage history under the task id
`reflection:<run id>`. A session started with `--no-session` has no file to read and cannot
be reflected on.

#### Generated skills

When a run's slice is clean, the model may also propose skills: short task-scoped
procedures that you asked for or approved and that ran successfully in the slice. A
proposal becomes `~/.otto/skills/<name>/SKILL.md` only after all of these pass, in order.
Any failure drops the skill and is counted; nothing is partly written.

1. **Source isolation.** If the slice contains any external entry (see above), skills are not
   requested at all and the run says so. `skill_source = "any"` lifts this; external entries
   are then still withheld and cannot be cited.
2. **Evidence.** A skill must cite one of your messages and one entry showing the procedure
   ran (a successful tool result, or an assistant tool call), with verbatim quotes.
3. **Structure and rule scan.** A valid name and description, a body of 40 characters to
   16 KiB, and no match in a fixed rule table: secret patterns and any value Otto knows to
   be secret, instruction-override and conceal-from-the-user phrasing (English and
   Chinese), tampering with the sandbox, approvals, or `~/.otto` state, pipe-to-shell,
   destructive and data-egress commands, URLs that no cited entry contains, invisible
   Unicode, and large encoded blobs. This is a tripwire for known-bad shapes, not a proof of
   safety.
4. **Name and ownership.** A new skill never reuses a name that exists in any configured skill
   root. Reflection revises only a skill it wrote whose file is unchanged since; a skill you
   wrote, or a generated one you have since edited, is never revised or removed by it.
5. **Model review** (`skill_review`, default on). A second tool-less call sees only the
   candidate, never the transcript, and must answer `ALLOW`; an error, a timeout, or any other
   answer rejects the skill.

A generated skill carries only `name` and `description`: it never declares an `input`/`output`
contract, so it is never run as a sub-agent, and it has no `allowed-tools`. At most
`max_skills` are written per run and `max_generated_skills` in total. Each write is announced
on the `/reflect` result line with the undo command, and the skill becomes visible to the model
at the next catalog discovery (`/new`, `/resume`, `/model`, or a restart), like any new skill.
Skills are written only to `~/.otto/skills`; if you changed `[skills].paths` so that root is
not listed, they are written but not discovered.

- `/skill generated` lists the skills reflection wrote, with the run, the session, the time,
  and the reason it gave; one you have edited is marked as no longer owned.
- `/skill <name>` on a generated skill shows where it came from.
- `/skill revert <name>` restores the previous version, or removes the skill if reflection
  created it. It refuses a skill you have edited. Every version reflection wrote is kept under
  `~/.otto/skill-history/`, outside the skill roots, so it is never loaded.
- `/skill set <name> disabled` also works on a generated skill.

The residual risk is stated plainly: a skill is instructions the model follows in later
sessions, and the checks above reduce but cannot remove the chance that a plausible-looking
bad procedure is written. Set `skills = false` to remove it. Under the default settings a
slice that held external content never produces skills.

```toml
[reflection]
enabled = true            # default true; false disables all reflection and does not open reflection.db
auto = "on_compaction"    # "on_compaction" (default) | "on_exit" | "off"
min_turns = 4             # 1..1000; on_exit skips a session with fewer user messages
memories = true           # default true; false stops memory proposals
skills = true             # default true; false never asks for or writes skills
skill_source = "untainted"  # "untainted" (default) | "any"
skill_review = true       # default true; the second-model review of each skill
max_input_bytes = 204800  # 1024..4194304; the oldest entries are cut to fit
max_memories = 8          # 1..8 memory proposals per run
max_skills = 2            # 1..8 skills per run
max_generated_skills = 30 # 1..200 skills reflection may own in total
```

With `memories = false` and `skills = false`, reflection makes no model call.

#### Automatic reflection

Reflection also starts by itself, as `[reflection].auto` selects. **It is on by default**
(`on_compaction`): upgrading adds one model call per automatic run in long sessions, and a run
may write skills (see above). Set `auto = "off"` to keep only `/reflect`, `skills = false` to
stop skill writing, or `enabled = false` to turn reflection off entirely.

- `on_compaction` (default): after a compaction completes, whether it started automatically or
  with `/compact`, a run starts in the background over everything no earlier run covered.
  It does not wait for or interrupt the turn, and at most one runs at a time. A session that
  never compacts never reflects on its own. When the run queued a candidate or wrote a skill,
  one line says so, printed before the next prompt (REPL) or added to the transcript (TUI
  and Web UI). A run that found nothing says nothing; a failed one shows its error once.
  `otto acp` runs it but has nowhere to show the line, so use `/memory review` and
  `/skill generated` to see the result.
- `on_exit`: when a terminal frontend (the TUI, the REPL, or `--prompt`) ends normally,
  reflection runs once over what no earlier run covered, if the session has at least
  `min_turns` user messages. **This delays exit** by as long as the run takes, up to 60
  seconds, while a line says so; Ctrl+C skips it. A killed process, a Ctrl+C exit, `otto serve`
  and `otto acp` do not run it. A session shorter than `min_turns` is skipped with no record, so
  `/reflect` can still cover it.
- `off`: only `/reflect`.

Limits that apply to automatic runs only: at least ten minutes between two automatic runs of
the same session (counted from when the earlier one started, and a failed run counts, so
nothing retries); and every other check above still applies. `/reflect` is never held back by
these limits.

Not yet implemented:

- `Binding.Observe` is not wired; reflection reads the session file instead.
- The Web UI and the HTTP API cannot review memory candidates; use `/memory review` in a terminal.
- `otto acp` does not show reflection notices.
- Reflection never changes a skill you wrote, and it cannot create skills with scripts or
  supporting files.
- No backup/restore/verify commands.
- No `otto memory backup|backups|verify|restore` subcommands.

## Skills

Otto loads reusable instruction sets ("skills") from `~/.otto/skills` (user level)
and workspace `.otto/skills` directories. A skill is a directory containing
`SKILL.md` with YAML frontmatter and Markdown body, following the Agent Skills
format.

Config (`[skills]` in TOML; all keys optional). Bundled skills installed by
`make install` go to `~/.otto/skills`, so they are available from every
workspace after starting a new Otto session:

```toml
[skills]
enabled = true                              # default true
paths = ["~/.otto/skills", ".otto/skills"]  # default; later entries win on name conflict
disabled = []                               # skill names disabled without removing their files
```

What's wired:

- Skill listing in the system prompt, capped at 8 KiB. Every skill stays
  listed: when the full entries do not fit, the whole listing steps down to a
  shorter form — first dropping locations, then truncating descriptions, then
  names alone — and one stderr warning names the form it settled on. A skill is
  only omitted when even the bare names overflow, and that warning names the
  omitted skills, because a skill the model never sees cannot be called by
  name. The sub-agent listing follows the same rule.
- A skill name in `disabled` remains on disk but is excluded from the model-visible
  listing, the `skill` tool, and skill-derived sub-agent definitions after Otto
  restarts. Use `/skill set <name> enabled|disabled` to maintain this list.
- The `skill` tool for the model to load instructions by name or read supporting
  files.
- Automatic appending of existing skill roots to Seatbelt read paths at process
  start.
- Validation: `name` equals the directory name (`a-z`, `0-9`, `-`; 1 to 64
  characters) and `description` is 1 to 1024 characters. Invalid skills print
  one stderr warning and are skipped.
- Optional `input` and `output` frontmatter keys, each up to 1024 characters,
  declaring what the skill expects to be given and what it returns. They must
  be declared together; declaring one alone, leaving one blank, or exceeding
  the bound prints one stderr warning and the skill keeps working as if
  neither were declared. `/skills` marks a skill that declares both with
  `[contract]`. They are Otto's own addition to the Agent Skills format, so
  other tools ignore them. See the
  [sub-agent execution design](specs/2026-09-22-skill-subagent-execution.md)
  for the reasoning.
- A skill that declares a contract is also registered as a sub-agent
  definition under its own name, so the model may run it with the `agent`
  tool: the skill's instructions and the delegated task go into a fresh
  context, and only the result comes back. The skills listing marks it
  `exec="agent"`, and the Agents listing carries the contract, which is what
  the model needs to decide what to send. Loading such a skill inline with
  the `skill` tool stays available, because combining two skills is what
  sub-agent execution gives up.
- The skill cannot widen what the sub-agent may do: it supplies only the
  instructions, never the tool set, the model, or the write policy. An
  `AGENT.md` definition of the same name wins, and the clash is reported.
  Registration is skipped entirely when `[agents]` is disabled, and a skill
  is never marked `exec="agent"` unless it really was registered.
- `/skill generated` and `/skill revert <name>` list and undo the skills that
  [reflection](#reflection) wrote.
- `/skill` lists every available skill; `/skill <name>` displays its description,
  location, contract-check status, and instructions; `/skill set <name>
  enabled|disabled` persists its state change. `/skills` is not a command.
- Discovery runs at startup and on `/new`, `/resume`, `/model`; the catalog is
  fixed within a session. A loaded body is a normal tool result stored in the
  session and re-sent on every later request until compaction.

Not yet implemented:

- `allowed-tools` enforcement.
- Reading `~/.claude/skills`; hot reload inside a session.

### Automatic contract check (experimental)

This feature is experimental: its config, storage, and display may change or
be removed without notice.

When enabled, the first time a session delegates to a given version of a
skill's `SKILL.md` (a skill with an `input`/`output` contract, run through the
`agent` tool), Otto sends the full `SKILL.md` text, including its
frontmatter, to `https://api.typesafe.ai` for an automated review of whether
the skill's instructions are self-contained and its declared contract
matches what the body does. A skill flagged by a local pattern check (for
example, an `output` field too vague to be useful) is judged locally and
never sent. Results are stored in `~/.otto/skill-checks.db` (SQLite,
append-only) and shown in `/skill <name>` for contract skills.

Enable it with all three of:

```toml
[skills]
enabled = true            # default; must not be explicitly false

[experimental]
typesafe_skill_check = true
```

and a non-empty `TYPESAFE_API_KEY` environment variable. Any one of these
missing leaves the checker off entirely: nothing is sent, and
`~/.otto/skill-checks.db` is never opened.

## MCP servers

Otto connects to Model Context Protocol (MCP) servers over stdio (a local
subprocess) or Streamable HTTP, and registers each server's tools for the
model to call in the same turn loop as the built-in tools.

You can edit TOML directly, or use `otto mcp add` to write the server table
for you. The commands below write the default config file
`~/.config/otto/config.toml` and print a reminder to restart Otto:

```bash
otto mcp add github \
  --transport stdio \
  --command npx \
  --arg -y \
  --arg @modelcontextprotocol/server-github \
  --env GITHUB_TOKEN=GITHUB_TOKEN

otto mcp add docs \
  --transport http \
  --url https://mcp.example.com/mcp \
  --header 'Authorization=Bearer ${DOCS_MCP_TOKEN}'

otto mcp add remote \
  --transport http \
  --url https://remote.example.com/mcp \
  --auth oauth \
  --scope mcp:tools
```

`--env KEY=ENVVAR` stores `KEY = "${ENVVAR}"`, not the current environment
value. For HTTP headers, pass the desired header value; use `${VAR}` in that
value for secrets. Other configuration commands are:

```bash
otto mcp list
otto mcp disable github
otto mcp enable github
otto mcp remove github
```

After adding or changing a server, restart Otto before expecting the running
session to see new tools. For OAuth HTTP servers, run `otto mcp login <server>`
and then restart.

Manual config (`[mcp]` and `[mcp.servers.<name>]` in TOML):

```toml
[mcp]
enabled = true              # default true; false skips every server
call_timeout_secs = 60      # default 60; per tool call, and cancels the call on the server
connect_timeout_secs = 20   # default 20; bounds the whole connect (discover probe, handshake, and tool listing together)

[mcp.servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
cwd = "."                    # default: the workspace path

[mcp.servers.docs]
transport = "http"
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_MCP_TOKEN}" }

[mcp.servers.remote]
transport = "http"
url = "https://remote.example.com/mcp"
auth = "oauth"               # default "none"
oauth_client_id = "otto"     # optional; used only if the server has no dynamic registration
oauth_scopes = ["mcp:tools"] # optional

[mcp.servers.legacy]
transport = "stdio"
command = "./bin/legacy-server"
enabled = false              # declared but not connected
```

Rules:

- Server names match `^[A-Za-z0-9_-]{1,32}$`.
- `${VAR}` and `${VAR:-default}` in `env` values, `headers` values, `url`,
  `command`, `args`, and `cwd` expand from the process environment at startup.
  An unset variable without a default is a configuration error. Credentials never appear verbatim in `config.toml`.
- A stdio server's child process gets exactly the `env` table plus `PATH`,
  `HOME`, `TMPDIR`, `LANG`, and `TERM` copied from Otto's own environment; no
  other variables are inherited. **stdio servers run unsandboxed**, outside
  Seatbelt.
- An MCP tool is registered as `mcp__<server>__<tool>`; non-`[A-Za-z0-9_-]`
  bytes in the tool name are replaced with `_`. If the result would exceed 64
  bytes, collides with another tool on the same server, or is already
  provided by an earlier-connected server, the tool is skipped and a warning
  is printed instead.
- Only `${VAR}` substitutions in `env` and `headers` values are treated as
  secrets and redacted from tool output; substitutions in `command`, `args`,
  `url`, and `cwd` are not, since they are not exposed to the model. A server
  whose `env`/`headers` secrets exceed the redaction limits (64 values, 8 KiB
  each, 16 KiB total) is reported `failed: ...` at startup instead of being
  connected.

What's wired:

- Otto connects every enabled server concurrently and then applies the
  outcomes in configuration order, so warnings, `/mcp` rows, and tool-name
  deduplication do not depend on which server answered first. A server that
  fails to connect (bad command, connection refused, handshake
  timeout) is reported as `failed: <reason>` and contributes no tools; the
  runner still starts with every other tool available. Connected MCP servers
  expose a small lazy router (`mcp_search_tools` and `mcp_call_tool`) instead
  of registering every remote tool schema in the model context. Search for a
  remote tool by server/name/description, then call the selected tool by the
  full `mcp__<server>__<tool>` name returned by search.
- A TUI or REPL session starts before its MCP servers are connected: the
  prompt is usable immediately, `/mcp` reports every enabled server as
  `connecting`, and the MCP router tools are attached in one swap once every
  server has settled. The swap happens between turns, never inside one, so a
  turn either has the MCP router tools or does not. A headless `--prompt` run
  and `otto serve` connect before the first turn instead.
- An HTTP server configured with `auth = "oauth"` that has no valid stored
  token is reported as `needs login`, contributing no tools, until `/mcp
  login <server>` (or `otto mcp login <server>`) completes and Otto is
  restarted.
- `/mcp` (REPL and TUI) prints one line per configured server: its connection
  state (`connected (N tools)`, `connecting`, `disabled`, `needs login`, or
  `failed: <reason>`), transport (`stdio` or `http`), and protocol era
  (`modern`, `legacy <version>`, or `-` for a server that never negotiated
  one).
- `/mcp login <server>` runs the OAuth authorization code flow for one
  configured HTTP server with `auth = "oauth"`, opens the authorization URL,
  and stores the resulting token under `~/.otto/auth/mcp/<server>.json`.
  Otto must be restarted afterward to connect with the new token; the running
  session keeps reporting `needs login` until then.
- `otto mcp list`, `otto mcp add`, `otto mcp remove`, `otto mcp enable`, and
  `otto mcp disable` manage server declarations in the default config file
  without starting a session. They do not connect servers in an already-running
  Otto process; restart Otto to apply the change.
- `otto mcp login <server>` and `otto mcp logout <server>` run the same sign-in
  flow, or remove the stored token, without starting a session. See
  [command-line reference](#command-line-reference).
- Tool results are text-only: `image`/`audio` content blocks, and a
  `resource` block with no `text` field, become a one-line placeholder naming
  the MIME type and byte count; binary content is never embedded. A
  `resource` block that does carry a `text` field returns that text
  verbatim. `env`/`headers` secrets (see Rules above) are redacted from tool
  output before it reaches the transcript.

Not yet implemented:

- Restarting an exited stdio server. Once a connected stdio server's process
  exits, it stays disconnected for the rest of the session; restart Otto to
  reconnect.
- Running stdio servers under the Seatbelt sandbox.
- Reloading `[mcp]` without a restart, and re-registering tools in a running
  session after `/mcp login`.
- Resources, prompts, and per-tool approval prompts.

## Troubleshooting

### `otto: missing api key`

Export the environment variable named by the selected profile's `api_key_env`,
or set `OTTO_API_KEY` as a fallback.

### `otto: missing base_url`, `otto: invalid base_url`, or request failures

Check the selected profile, `--base-url`, and endpoint path. Otto posts to
`<base-url>/chat/completions`.

### `read chat completion stream: ...` or stream ended without `[DONE]`

The provider or proxy is not delivering valid SSE chat-completions output.
Confirm streaming is enabled and SSE is not buffered or rewritten.

### `warning: bash is unavailable because the configured sandbox could not be established ...`

On macOS, `auto` means Seatbelt. On Linux there is no confined driver at all,
so this warning names `unsupported-platform` and `--sandbox off` is the only
way to run commands (see [platform support](#platform-support)). Otto does not
fall back to Docker or direct execution unless you explicitly choose
`--sandbox off` (or `driver = "off"` in config). Common fixes:

- confirm `/usr/bin/sandbox-exec` is present and usable;
- narrow `read_paths` so they do not include Otto's private sandbox cache root;
- move the selected workspace outside cache-like locations if it would overlap
  Otto's private sandbox state;
- prefer narrow `read_paths` plus exact config variables over broad home or
  cache access;
- use `--sandbox off` only if you accept unsandboxed current-user execution.

### `no chatgpt credentials; run 'otto login'`

The `chatgpt` provider has no stored OAuth credentials. Run `otto login` to sign
in with your ChatGPT subscription, or check state with `otto login --status`.
See [ChatGPT subscription](#chatgpt-subscription).

### `/mcp` reports `failed: ...` or `needs login`

Check `/mcp` for the exact reason. `failed: <reason>` means the stdio command
could not be spawned, the HTTP connection, handshake, or tool listing did not
complete within `connect_timeout_secs`, or the server's `env`/`headers`
secrets exceed the redaction limits; the server contributes no tools until
Otto is restarted with the problem fixed. `needs login` means the server
requires OAuth and has no valid stored token: run `/mcp login <server>` (or
`otto mcp login <server>`), then restart Otto. See
[MCP servers](#mcp-servers).

### Context-length or prompt-size failures

Otto tries one automatic checkpoint before the hard limit (when it knows the
model window) and one typed-overflow recovery checkpoint after a provider
context error. If you still hit a hard input limit:

- run `/compact [focus]` to create a manual checkpoint,
- use `/new` for a completely fresh session,
- reduce large pasted input or large tool output,
- set profile `context_window` / `compaction_window` for private or unknown
  model IDs,
- or choose a model/profile with a larger working window.

### A session disappeared from `/resume` or `--continue`

The session was likely archived. Archive moves the file (not deletion) into
`~/.otto/sessions/<workspace-key>/archive/<session-id>.jsonl`. To reopen it:

```bash
./otto --cwd /path/to/project --resume ~/.otto/sessions/<key>/archive/<session-id>.jsonl
```

The file is still intact; only the active-session surfaces (`/resume`,
`--continue`, and the `/archive` picker) exclude it.

### A session takes too long to start

Set `OTTO_STARTUP_TRACE=1` (`true`, `yes`, and `on` also work) to print one
line per startup phase to stderr, ending with the total time to a usable
prompt:

```bash
OTTO_STARTUP_TRACE=1 otto --cwd /path/to/project
```

```text
startup environment: 0ms
startup config/load: 1ms
startup sandbox/open: 214ms
startup memory/open: 3ms
startup session/activate: 0ms
startup runner/setup: 0ms
startup runner/catalogs: 2ms
startup runner/mcp: 0ms
startup runner/provider: 1ms
startup runner/workspace-context: 41ms
startup runner/subagents: 0ms
startup runner/registry-and-options: 0ms
startup runner/build: 44ms
startup ready: 265ms
```

Each line is the time that phase took. The `runner/*` lines break the runner
build down, and `runner/build` is their total; `startup ready` is the whole
elapsed time before the frontend starts. `runner/mcp` is near zero in an
interactive session because MCP servers connect in the background after the
prompt is usable (see [MCP servers](#mcp-servers)), and
`runner/workspace-context` includes the `git` calls the workspace header
makes.
