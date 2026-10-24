# Getting started

[Documentation index](../README.md) · Next: [Interactive use](interactive-use.md)

## Prerequisites

Install the toolchains pinned in `rust-toolchain.toml` and
`chat-connector/go.mod` (Rust and Go), then configure either an
**OpenAI-compatible** endpoint or the **ChatGPT** provider. These are the only
supported providers. Model requests leave your machine; sessions, configuration
and default memory remain local.

### Platform support

macOS is the supported and acceptance-tested platform. Its default shell
sandbox uses Seatbelt. Linux builds and runs Otto but has no confined shell
driver: `auto` and `seatbelt` disable `bash` rather than silently running it
unconfined. File tools remain available. `--sandbox off` explicitly grants
unconfined shell access; do not use it merely to silence a warning. See
[Safety and tools](safety-and-tools.md#bash-sandbox-policy).

## Quick start

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make install
otto setup
otto --help
otto --cwd /path/to/project
```

`make install` installs `otto` and `otto-connect` in `~/.local/bin/` and bundled
skills in `~/.otto/skills/`. Add `~/.local/bin` to `PATH`. For a checkout-local
binary use `make build`, then `./otto`. Upgrade by updating the checkout,
rerunning `make install`, and restarting affected runtimes and connectors.
Browser UI development/hosting is separate: [otto-plus](https://github.com/baiyuqing/otto-plus).

For an OpenAI-compatible connection, keep the API key in the environment, not
TOML or a prompt:

```bash
export OPENAI_API_KEY='…'  # set privately; avoid saving a real key in shell history
otto --provider openai-compatible --base-url https://api.example.com/v1 --model MODEL_ID
```

Use a model ID supported by your endpoint. `otto setup` can create a provider
profile. `otto --help` and each subcommand's `--help` are the complete CLI
reference; the tables below highlight daily entry points.

### Launcher script

```bash
cp otto.env.example otto.env
make run                    # or scripts/otto.sh
```

The ignored `otto.env` is sourced as a shell profile, **not** parsed as inert
data: review it before use and never commit secrets. It keeps provider, model,
workspace and sandbox startup options together. Environment changes affect new
processes, not already-running `otto serve` or ACP processes.

### Docker on Mac

With Docker Desktop running:

```bash
docker compose build
docker compose run --rm otto setup
docker compose up -d
docker compose logs otto
```

Use the printed endpoint/token with an API client or otto-plus. The image serves
the API and builds on Apple Silicon and Intel Macs. A named volume retains
configuration and sessions; `OTTO_WORKSPACE` selects the host directory mounted
into the container. Review `compose.yaml` and `otto.env.example` before changing
mounts or listener exposure. A Linux container does not provide macOS Seatbelt;
container isolation and a narrowly scoped workspace mount are its boundary.
Rebuild the image and recreate the service after upgrades; preserve its volume.

Create the host workspace first and allow it in Docker Desktop file sharing;
the container runs as UID/GID 1000 and needs write access. In setup choose the
API-key variable that Compose forwards, or add the profile's selected variable
to the service `environment` mapping. Export it before `compose up`; it is not
a build argument. Use `host.docker.internal`, not container `localhost`, for a
model endpoint on the Mac. The published API is Mac-loopback only, retains
bearer authentication, and gets a new token after each server restart.

Compose does not publish ChatGPT OAuth callback ports. Sign in natively, select
a ChatGPT profile in container setup, and import private login state into its
volume before starting:

```bash
otto login
docker compose run --rm --no-deps -T --entrypoint sh otto -c \
  'umask 077; mkdir -p "$HOME/.otto/auth"; cat > "$HOME/.otto/auth/chatgpt.json"' \
  < "$HOME/.otto/auth/chatgpt.json"
docker compose run --rm --no-deps otto login --status
```

Never add that file to the image/build context; native/container logouts affect
their own copies. `docker compose run --rm otto --cwd /workspace` opens the TUI
with the same state/workspace. No connector is included in this deployment;
install/configure `otto-connect` separately.

## ChatGPT subscription

### Signing in

```bash
otto login
otto login --status
otto logout
```

Follow the browser/device instructions from `otto login`. ChatGPT credentials
come from that login flow, not an API-key variable or config file. Run login on
the host/account that runs Otto, including the server when attaching remotely.

### Using the subscription

```bash
otto --provider chatgpt --model MODEL_ID
```

Use a model available to the signed-in account. ChatGPT account availability and
limits apply; an API subscription/key is a different connection.

### How it works

Otto uses the signed-in account's authenticated ChatGPT connection. Credentials
are local authentication state and must not be copied into workspace files,
logs, prompts or examples. If startup reports missing credentials, run
`otto login` in the runtime's environment and restart it. See
[Troubleshooting](troubleshooting.md).

## Command-line reference

| Entry point | Purpose |
| --- | --- |
| `otto setup` | Guided provider/profile setup |
| `otto --cwd DIR` | Start in a selected workspace |
| `otto --profile NAME` | Select a configured connection |
| `otto --provider … --model …` | Override the startup connection/model |
| `otto --prompt TEXT` | One headless turn; combine with `--no-session` if persistence is unwanted |
| `otto --continue` / `--resume PATH` | Reopen history for the current workspace |
| `otto --archive PATH` | Archive one active session and exit |
| `otto --no-session` | Keep conversation history in memory only |
| `otto --config PATH` | Select the user configuration source |
| `otto config explain --cwd DIR --json` | Inspect resolved configuration offline |
| `otto config set KEY TOML_VALUE --scope user\|shared\|local` | Edit one source |
| `otto trust DIR` | Admit a workspace to the server; not executable trust |
| `otto serve` | Shared local agent server |
| `otto acp` | ACP agent server |
| `otto workflow --help` | Durable workflow lifecycle commands |
| `otto memory --help` | Local memory inspection/management |
| `otto --version` | Version and build identity |

See [interactive commands](interactive-use.md#slash-commands),
[servers](servers-and-connectors.md), [automation](automation.md), and
[tools/permissions](safety-and-tools.md). Never pass credentials on the command
line: process listings and shell history can expose them.

## Environment variables

Use the environment binding selected by the active provider profile
(`api_key_env` for OpenAI-compatible profiles). MCP, chat and plugin package
credentials likewise come from their configured environment bindings, never
literal secrets in configuration. `otto login` manages ChatGPT credentials.

Set variables in the process that actually performs the work. An attaching
terminal's environment does not replace the server's; a supervisor, container
or connector may have a different environment. Restart that process after
changing credentials. Inspect the launcher template and relevant subsystem's
configuration for supported variable names rather than guessing them.

## Configuration

Ordinary configuration resolves highest first: **managed policy → local
project → shared project → user → built-in defaults**. `--config PATH` changes
the user source, not project discovery or managed policy. Review a selected
user source: it can grant shell access and configure executables/credentials.

| Source | Path |
| --- | --- |
| User | `~/.config/otto/config.toml`, or `--config PATH` |
| Shared | `.otto/config.toml` from Git worktree root to workspace |
| Local | `.otto/config.local.toml` along the same directory chain |
| Managed | Linux: `/etc/otto/policy.toml`; macOS: `/Library/Application Support/Otto/policy.toml` |

Nested Git checkouts start new boundaries. Outside Git only the selected
workspace contributes project config. All shared files merge root-to-leaf,
then all local files root-to-leaf: root local wins over deeper shared. Ignore
local files in Git yourself. Unsafe paths or malformed discovered files fail
loading.

Scalars override, tables merge recursively, and lists replace (`[]` clears).
Profiles/MCP servers merge by name then field. There is no `+item`, `-item`,
`null` or deletion syntax. Use `enabled = false` where supported. Changing an
MCP transport removes its former transport's fields.

Projects may set `default_profile`, UI mode, agent limits/deadlines and profile
model/thinking/context/compaction windows. Provider identity/URL, credentials,
sandbox grants, listeners, admission, storage and catalog roots
require user/managed sources. Ignored fields produce diagnostics. Projects
cannot retarget credential-bound MCP servers, interpolate environment variables
in project MCP definitions, or supply their env, headers or OAuth bindings.
Plugin package bindings (`[plugins.bindings.*]`) are also host-only.

### Inspecting and editing scopes

```bash
otto config explain --cwd ./project --json
otto config set ui.mode '"repl"' --scope local --cwd ./project
otto config set profiles.work.model '"MODEL_ID"' --scope shared --cwd ./project
otto config set default_profile '"work"' --scope user
```

`explain` is offline: no provider/MCP calls, hooks or credential resolution. It
shows origins, overwritten/ignored fields, executable trust and constraints;
credential bindings, URLs and arguments are redacted. `effective_runtime` is
credential-free and null when provider/model resolution is invalid. Dotted
leaf paths match `config set` keys.

`set` needs an explicit scope and TOML value, changes only that source, backs
up old bytes and refuses concurrent changes. It does not serialize the merged
configuration. A shadowed value stays shadowed: inspect `explain` after edits.
Other management commands write their user source. Restart affected runtimes
for ordinary configuration changes.

### Project executable trust

Workspace admission (`otto trust DIR`) does **not** authorize project MCP,
skills or agent definitions. Executable trust is a separate content-bound
review: grant with `otto trust DIR --executables`, revoke with
`otto trust DIR --revoke-executables`, and use `otto config explain` to inspect
trust. Changing reviewed executable content
requires a new review. Project configuration and model messages cannot grant
this authority. Do not approve unfamiliar project executables just to remove
a startup diagnostic.

### Managed policy

Managed policy is administrator-controlled, highest-precedence configuration.
Its file and executable paths must have safe ownership/ancestors; do not copy
an untrusted project file there. It can constrain lower configuration and
permission rules. Use `config explain` to diagnose constraints rather than
weakening the policy or trying another project scope.

A small user configuration illustrates the common settings:

```toml
default_profile = "work"

[ui]
mode = "tui"

[profiles.work]
provider = "openai-compatible"
base_url = "https://api.example.com/v1"
model = "MODEL_ID"
api_key_env = "OPENAI_API_KEY"

[sandbox]
driver = "auto"
network = "allow"
read_paths = []
allow_env = []

[agent]
shell_timeout = "120s"
max_output_bytes = 51200
# Optional deadlines: turn_timeout, provider_timeout, subagent_timeout,
# workflow_step_timeout. The unattended deadline defaults to 30m.
# unattended_turn_timeout = "0"  # explicitly disable it

[agent.compaction]
auto = true
reserve_tokens = 16384
keep_recent_tokens = 20000

[skills]
enabled = true
paths = ["~/.otto/skills", ".otto/skills"]

[server]
socket = "~/.otto/otto.sock"
# listen = "127.0.0.1:8787"
```

### Precedence

Command-line connection/model choices and session commands affect the runtime;
ordinary stored values still follow the source rules above. Resuming history
does not restore its historical model settings. Select an intended profile
rather than mixing provider/endpoint/credential overrides accidentally.

Subagents normally use authenticated `list_models` discovery, falling back to
the session model if discovery is unavailable or suitability unknown. Explicit
user model choices and named-agent defaults take precedence. The optional,
deprecated user-profile `subagent_models` list overrides catalog selection; it
is not required and is disabled by a change to its provider connection. See
[`list_models`](safety-and-tools.md#list_models).

Deadlines use Go-duration strings; `0` disables an optional condition. Most
end-to-end deadlines are disabled when omitted, except unattended chat/headless
turns (30 minutes). Cancellation cannot prove an external side effect did not
occur. Synchronous filesystem/storage calls are checked before and after, not
forcibly detached. See [recovery](troubleshooting.md) before retrying a write.

For detailed sandbox exclusions, environment filtering, approval defaults,
network/read grants and unsandboxed access use [Safety and tools](safety-and-tools.md).
For memory, MCP, hooks and custom tools use
[Memory and extensions](memory-and-extensions.md). Restart long-lived runtimes
after changing their configuration or environment.

### Backups of the config file

Configuration management backs up the previous file before replacing it and
refuses conflicting concurrent edits. Keep backups private: old files can
contain sensitive connection information. To recover, stop affected runtimes,
inspect the desired backup, restore only the intended user source, run
`otto config explain --json`, then restart. Never replace a project source with
a merged dump or restore old credentials into source control.
