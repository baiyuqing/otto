# Getting started

Previous: — · [Documentation index](../README.md) · Next: [Interactive use](interactive-use.md)

## Prerequisites

- macOS, the supported platform: it is the only one with a sandbox, and the
  only one the acceptance gate runs on. Otto also builds and runs on Linux;
  see [platform support](getting-started.md#platform-support).
- The pinned Rust 1.99 toolchain and Go to build and install from source (see
  [Install from source](../../README.md#install-from-source)). Node and `wasm-pack`
  are needed only for optional browser/WASM development and related checks.
- One of:
  - a reachable OpenAI-compatible endpoint with SSE chat-completions streaming, plus an API key exposed through an environment variable, or
  - a ChatGPT Plus/Pro/Team/Enterprise subscription (see [ChatGPT subscription](getting-started.md#chatgpt-subscription)).

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
found. The browser that `otto login` launches is
`/usr/bin/open` on macOS and `xdg-open` on Linux; a launch that fails is never
fatal, because the URL is printed either way. A `bash` command runs under a
login shell (`sh -lc`) on macOS, so it sees the `PATH` `path_helper` assembles
from `/etc/paths` and `/etc/paths.d`; elsewhere it runs as `sh -c`, because
`/etc/profile.d/*` would write into the tool result the model reads and could
restore variables the environment filter removed.

### Docker on Mac

Install and start Docker Desktop, then clone this repository. Docker handles
the Rust toolchain inside a multi-stage build; it need not be installed on
your Mac. The image serves the Otto API and includes Bash, Git, CA certificates
and the bundled sandbox-setup skill. Use the separate
[otto-plus](https://github.com/baiyuqing/otto-plus) project for the browser
frontend.
`otto-connect` is installed separately.

From the checkout directory:

```bash
docker compose build
docker compose run --rm otto setup
export OTTO_WORKSPACE="$HOME/my-project"
docker compose up -d
docker compose logs otto
```

Create the workspace directory before starting. Without `OTTO_WORKSPACE`,
Compose mounts the checkout itself. For an OpenAI-compatible profile, use
your endpoint/model and select `OTTO_API_KEY` as the key environment variable
in setup. Export that variable in the shell before `docker compose up -d`;
Compose passes it to Otto. If a profile uses a different key variable, add
that variable to the service's `environment` mapping. Keys are never build
arguments or TOML configuration. To use a model server on your Mac, its
container-accessible hostname is `host.docker.internal`, rather than
`localhost` inside the container.

For ChatGPT, this Compose setup does not publish the OAuth loopback callback
ports used by `otto login`. Sign in with a native Otto installation on your
Mac first, select a `chatgpt` profile in container setup, then import the
credentials into the container's state volume before starting the server:

```bash
otto login
docker compose run --rm --no-deps -T --entrypoint sh otto -c \
  'umask 077; mkdir -p "$HOME/.otto/auth"; cat > "$HOME/.otto/auth/chatgpt.json"' \
  < "$HOME/.otto/auth/chatgpt.json"
docker compose run --rm --no-deps otto login --status
```

The imported file is private authentication state, never part of an image
or build context. Native and container logouts affect their own copies.

Use the host/port and token printed by `docker compose logs otto` with an
API client or [otto-plus](https://github.com/baiyuqing/otto-plus). Otto generates a new token each time the
server starts. A separate TCP relay shares its network namespace and
forwards container port 8788 to Otto's loopback listener on port 8787.
Only Mac loopback port 8787 is published; the API retains its bearer-token
authentication. This is a local Docker Desktop setup, not a public service.

The `otto-state` named volume mounts at `/home/otto` and preserves
configuration in `.config/otto`, plus sessions, memory and login state in
`.otto`. The process runs as UID/GID
1000, with `/workspace` as its working directory. Mounted projects must be
writable by that user; Docker Desktop file sharing must allow the selected
Mac directory. The relay has a read-only filesystem and no project or state
mounts. To run the TUI with the same configuration and workspace:

```bash
docker compose run --rm otto --cwd /workspace
```

The CLI is also available without Compose:

```bash
docker build -t otto:local .
docker run --rm -it \
  -v otto-cli-state:/home/otto \
  --mount "type=bind,src=$PWD,dst=/workspace" \
  otto:local setup
docker run --rm -it -e OTTO_API_KEY \
  -v otto-cli-state:/home/otto \
  --mount "type=bind,src=$PWD,dst=/workspace" \
  otto:local --cwd /workspace
```

Those direct commands use their own `otto-cli-state` volume. The default
standalone image command displays CLI help. Docker builds for the host's
Linux architecture: `linux/arm64` on Apple Silicon, `linux/amd64` on Intel.
To build a specific architecture instead:

```bash
docker buildx build --platform linux/arm64 --load -t otto:local .
# Use --platform linux/amd64 for Intel.
```

To include the source revision in `otto --version`, pass
`--build-arg OTTO_GIT_HASH="$(git rev-parse --short=12 HEAD)"` to a direct
build, or export `OTTO_GIT_HASH` before `docker compose build`.

Docker containers run Linux even on Mac. The default sandbox setting keeps
Bash unavailable because Linux has no confined driver. To opt into Bash,
pass `--sandbox off` to CLI commands or append it to the `otto` service's
`command` list and recreate the services. Shell commands can then change
all writable mounted files; Docker isolation does not provide Seatbelt.
Never mount the Docker socket for ordinary Otto usage.

Stop with `docker compose down`. It retains the state volume. After code
updates, run `docker compose build` followed by
`docker compose up -d --force-recreate` to recreate both Otto and its relay.
`docker compose down -v` permanently removes the Compose state volume.

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

### Launcher script

In a source checkout, `scripts/otto.sh` starts Otto in the foreground with the
settings from `otto.env`, so the command line lives in one file instead of your
shell history. Copy the template and edit it:

```bash
cp otto.env.example otto.env
make run                 # same as scripts/otto.sh
```

`otto.env` is read as a shell file and ignored by Git, as are variants such as
`otto.env.local`, so quoting, `#` comments and `export` work. It is trusted
local configuration, like a shell profile; put the real key there only in your
own copy, never in the committed example. A checkout without `otto.env` still
starts: the script says so on stderr and Otto falls back to its own
configuration and the environment. An empty setting says nothing and behaves
the same way.

| Setting | Otto option |
| --- | --- |
| `OTTO_PROVIDER`, `OTTO_MODEL`, `OTTO_PROFILE` | `--provider`, `--model`, `--profile` |
| `OTTO_BASE_URL` | `--base-url` |
| `OTTO_CWD` | `--cwd` |
| `OTTO_UI` | `--ui` |
| `OTTO_SANDBOX` | `--sandbox` |
| `OTTO_THINKING` | `--thinking` |
| `OTTO_SHELL_TIMEOUT`, `OTTO_MAX_OUTPUT_BYTES` | `--shell-timeout`, `--max-output-bytes` |
| `OTTO_API_KEY` | Environment only; there is no key option |
| `OTTO_EXTRA_ARGS` | Appended verbatim, split on whitespace |

Options written after the script name are appended last and win over the file,
which is how a one-off run overrides it:

```bash
scripts/otto.sh --model other-model --prompt "summarize the diff"
```

`OTTO_ENV_FILE` selects another settings file and `OTTO_BIN` another binary;
both default to the checkout. The launcher is for the foreground frontends; the
`otto serve` agent server is started directly, as described under
[agent server](servers-and-connectors.md#agent-server), or as the pair described under [the ACP TCP
stack script](servers-and-connectors.md#the-acp-tcp-stack-script), which reads this same file. That
script ignores `OTTO_UI` and `OTTO_EXTRA_ARGS`, because `otto serve` accepts
neither `--ui` nor `--prompt`; `OTTO_SERVE_EXTRA_ARGS` is its equivalent of the
latter.

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
  format from the OpenAI-compatible Chat Completions provider. ChatGPT uses the
  backend's Responses compaction protocol; other providers use Otto's local
  structured summaries.
- The access token is refreshed automatically from the stored refresh token
  when it nears expiry; rotated tokens are written back to the credential file.
- Tokens are never written to TOML, session files, or logs, and are stripped
  from provider error messages.
- Each request carries `prompt_cache_key` set to the session id (a sub-agent
  uses its own transcript id) so the backend can route a session's requests to
  the same prompt cache; the OpenAI-compatible provider does not send it.
- Exchanging the login for an API key (which would bill as API credits rather
  than subscription quota) is intentionally not supported.

## Command-line reference

Otto also has subcommands that run before the flags below are parsed:

| Command | Description |
| --- | --- |
| `otto login [--status]` | Sign in with a ChatGPT subscription, or (`--status`) report sign-in state. See [ChatGPT subscription](getting-started.md#chatgpt-subscription). |
| `otto logout` | Remove stored ChatGPT credentials. |
| `otto memory status\|forget <id>` | Inspect or delete memory records. See [Memory](memory-and-extensions.md#memory). |
| `otto sandbox setup [--config PATH] [--cwd PATH]` | Choose shell sandbox permissions interactively. See [Interactive sandbox setup](safety-and-tools.md#interactive-sandbox-setup). |
| `otto setup [--config PATH]` | Create an initial provider profile without storing credentials. See [Quick start](getting-started.md#quick-start). |
| `otto workflow run <name> [--input TEXT]` | Start a durable workflow and wait until it finishes or needs approval. |
| `otto workflow status <run-id>` | Print one workflow run and its approval requests as JSON. |
| `otto workflow resume <run-id> [--retry <step-id>]` | Resume safe pending work, or explicitly retry one interrupted step. |
| `otto workflow fork <run-id> --after-step <step-id>` | Create a new run from that step's committed success boundary. |
| `otto workflow approve\|reject <request-id>` | Resolve one persisted workflow approval gate. |
| `otto workflow cancel <run-id>` | Stop active attempts, then mark the run canceled. |
| `otto config explain [--cwd PATH] [--json]` | Inspect merged values, origins, ignored fields and executable trust offline. |
| `otto config set KEY TOML_VALUE --scope user\|shared\|local` | Edit a single explicitly chosen configuration source. |
| `otto plugin install\|list\|inspect\|enable\|disable\|trust\|revoke\|remove ...` | Manage Skills/MCP plugin packages. See [Plugin packages](memory-and-extensions.md#plugin-packages). |
| `otto mcp list` | List configured MCP servers and installed plugin servers. See [MCP servers](memory-and-extensions.md#mcp-servers). |
| `otto mcp add <server> ...` | Add an MCP server to `~/.config/otto/config.toml`. |
| `otto mcp remove <server>` | Remove one configured MCP server. |
| `otto mcp enable <server>` / `otto mcp disable <server>` | Toggle one configured MCP server. |
| `otto mcp login <server>` | Run the OAuth sign-in flow for one configured MCP server. See [MCP servers](memory-and-extensions.md#mcp-servers). |
| `otto mcp logout <server>` | Remove the stored OAuth token for one configured MCP server. |
| `otto trust <dir> [--config PATH] [--executables \| --revoke-executables]` | Record `<dir>` (canonicalized) as a trusted directory in the config file, so `otto serve` admits it and its descendants as workspaces. See [Workspaces](servers-and-connectors.md#workspaces). |
| `otto serve [--socket PATH] [--listen HOST:PORT]` | Run Otto as an HTTP+JSON+SSE agent server, over a Unix domain socket, a loopback TCP port, or both, instead of an interactive frontend. See [Agent server](servers-and-connectors.md#agent-server). |
| `otto acp [--attach [--socket PATH]]` | Run Otto as an Agent Client Protocol v1 agent on stdin and stdout, for ACP clients such as `otto-connect`. With `--attach`, forward each request to a running `otto serve` instead of opening sessions in this process. See [ACP agent server](servers-and-connectors.md#acp-agent-server). |
| `otto --attach [--socket PATH] [--resume ID \| --continue]` | Run the TUI as a client of a running `otto serve`, on a session that the web UI and other clients can use at the same time. See [The TUI attached to `otto serve`](interactive-use.md#the-tui-attached-to-otto-serve). |

| Flag | Description |
| --- | --- |
| `--help` | Show help and exit. |
| `--version`, `-V` | Print the version and exit. The output is `otto X.Y.Z+<commit>`, where `<commit>` is the 12-character git commit the binary was built from, with `.dirty` appended if tracked files had uncommitted changes at build time; `+unknown` means the build had no git information. |
| `--config PATH` | User configuration source. Defaults to `~/.config/otto/config.toml`; project discovery and managed policy still apply. |
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
| `--acp-listen HOST:PORT` | `serve` only. Additional authenticated ACP v1 TCP listener on loopback. Requires `OTTO_ACP_TOKEN` in the environment; see [Direct ACP TCP connection](servers-and-connectors.md#direct-acp-tcp-connection). |
| `--socket PATH` | `serve`, `acp --attach`, or `--attach`. Unix domain socket path of `otto serve`. Defaults to `[server].socket`, then `~/.otto/otto.sock`. For `serve` with `--listen`, both listeners are opened. |
| `--listen HOST:PORT` | `serve` only. Listen on a loopback TCP address instead of a socket and print the URL with the access token. Port `0` picks a free port. With `--socket`, both listeners are opened. |
| `--yolo` | Start this process's session in the YOLO approval mode, exactly as `/auto yolo` does. Requires a frontend that opens one session, so it cannot be combined with `serve`, `acp`, or `--attach`. |
## Environment variables

| Variable | Meaning |
| --- | --- |
| `OTTO_PROVIDER` | Provider override (overrides the profile; overridden by `--provider`). |
| `OTTO_PROFILE` | Profile override (overrides `default_profile`; overridden by `--profile`). |
| `OTTO_MODEL` | Model override (overridden by `--model`). |
| `OTTO_API_KEY` | Fallback API key, used when the selected profile's `api_key_env` variable is empty. |
| `OTTO_UI` | Frontend mode (`auto`, `tui`, `repl`); overridden by `--ui`. |
| `OTTO_STARTUP_TRACE` | `1`, `true`, `yes`, or `on` prints a startup timing breakdown to stderr (see [troubleshooting](troubleshooting.md#a-session-takes-too-long-to-start)). |
| `GOOGLE_API_KEY`, `GOOGLE_SEARCH_ENGINE_ID` | Google Custom Search JSON API key and search engine ID. When both are set, `web_search` uses Google; see [web tools](safety-and-tools.md#web-tools). |
| `BRAVE_API_KEY` | Brave Search API key. When set (and Google is not configured), `web_search` uses Brave. Without any key, `web_search` reads Bing's results page. |
| `<api_key_env>` | The variable named by the selected profile's `api_key_env`. Its value wins over `OTTO_API_KEY`. |

API keys are environment variables only. There is no `--api-key` flag, and keys
must never be stored in TOML.

## Configuration

Otto resolves ordinary configuration in this order, highest first:
managed organization policy, workspace-local, workspace-shared, user-global,
and built-in defaults. `--config PATH` selects the user source; it does not
remove project discovery or managed policy. Review explicitly selected user
sources: they can supply credentials, sandbox grants and executable settings.
For [remote memory](memory-and-extensions.md#remote-memory-service), configure
required `memory.remote.organization`, selected user and token binding in the
host user source. Organization/user/token authority cannot come from project
overlays or model arguments. The token value comes only from the configured
environment variable. Restart the affected runtime after configuration changes;
startup does not probe the external memory service.

| Source | Path |
| --- | --- |
| User | `~/.config/otto/config.toml`, or `--config PATH` |
| Shared | `.otto/config.toml` in each directory from the enclosing Git worktree root to the selected workspace |
| Local | `.otto/config.local.toml` along the same directory chain |
| Managed | `/etc/otto/policy.toml` on Linux; `/Library/Application Support/Otto/policy.toml` on macOS |

A nested Git checkout starts a new boundary. Outside Git, only the selected
workspace contributes project configuration. All shared files merge from root
to leaf, then all local files merge from root to leaf: a root local preference
wins over a deeper shared preference. Add local files to repository ignore
rules yourself. Unsafe paths and malformed discovered files fail loading.

Scalars override, tables merge recursively, and lists replace completely;
`[]` clears a list. Profiles and MCP servers merge by name and then by field.
There is no `+item`, `-item`, `null`, or deletion syntax. `enabled = false`
disables named entries that support it. Switching an MCP server's transport
removes fields belonging to its previous transport.

Project files may set `default_profile`, `ui.mode`, agent limits and deadlines,
and profile model/thinking/context/compaction windows. Provider identity,
provider URL, credentials, sandbox grants, server listeners, project admission,
storage, reflection and catalog roots are restricted to user/managed sources.
Ignored fields produce diagnostics. A project cannot change a credential-bound
MCP server's command, arguments, working directory, transport or destination.
Project MCP definitions cannot interpolate environment variables or supply
`env`, headers, OAuth or other credential bindings.

### Inspecting and editing scopes

```bash
otto config explain --cwd ./project --json
otto config set ui.mode '"repl"' --scope local --cwd ./project
otto config set profiles.work.model '"gpt-5"' --scope shared --cwd ./project
otto config set default_profile '"work"' --scope user
```

`explain` is offline: it does not start a provider, connect MCP, launch hooks,
or resolve environment credentials. Its JSON includes merged values, source
origins, overwritten origins, ignored fields, executable trust and managed
constraints. Environment bindings, URLs and executable arguments are redacted.
`effective_runtime` shows credential-free runtime defaults when the selected
provider/model is valid, and is `null` otherwise. JSON leaf paths use dotted
names, matching `config set`'s key syntax.

`set` requires an explicit scope and a TOML value. It edits only that source,
preserves supported comments/layout, backs up the previous bytes and refuses
concurrent changes. It never serializes the effective merged configuration.
Existing MCP, profile, sandbox and memory management commands continue to
write their user source. Values shadowed by another source remain shadowed;
inspect `explain` after editing. Restart affected workspace runtimes for
ordinary configuration changes. `--config PATH` applies to the user source
for both commands.

### Project executable trust

Directory admission and executable trust are separate. `otto trust DIR`
continues to admit a workspace to `otto serve`; it does not authorize project
MCP, skills or agent definitions. Preferences and instruction text load before
executable trust.

```bash
otto trust ./project --executables
otto trust ./project --revoke-executables
```

`--executables` displays discovered sources and records an explicit content
and path grant in `~/.config/otto/trust/executables.json`. Its backups stay in
the separate `trust/backups/` directory. Use the same `--config`
when granting trust for a custom user source. User-owned global catalogs keep
working; custom catalog roots inside the repository require this grant too.
Default catalogs load user roots first, then project roots from root to leaf;
later names win. Explicit catalog paths, including `[]`, retain their normal
replacement behavior.

Changing executable MCP definitions, skills, agents or their resources
invalidates the grant; ordinary UI/model changes do not. Grants reject unsafe
symlinks, non-regular resources, trees exceeding 4,096 files/32 levels,
resources larger than 1 MiB, and more than 16 MiB of resources in total.
Existing sessions check authorization before future tool effects. Revocation
also requests cancellation of running tools and closes MCP connections, with
checks every 500 ms where cancellation is supported. Restart after reviewing
and granting trust again; revocation cannot undo effects already completed.
Revoking a grant also works when the project configuration has become invalid.

### Managed policy

The fixed policy path and every ancestor must be administrator-owned and not
writable by group/others. Symlinks are rejected. A missing policy is optional;
an unreadable, unsafe or invalid policy fails closed. CLI flags, environment,
resumed sessions and sandbox reload cannot override managed constraints.
Policies constrain Otto itself; they do not replace operating-system controls.

Example administrator policy:

```toml
require_sandbox = true
allow_project_executables = false
allowed_provider_hosts = ["chatgpt.com"]
allowed_mcp_hosts = ["tools.example.org"]
allowed_credential_variables = ["TEAM_API_KEY"]
# Commands must be absolute, administrator-owned paths with safe ancestors.
allowed_mcp_commands = []

[force]
default_profile = "work"

[force.profiles.work]
provider = "chatgpt"
model = "gpt-5"

[ceilings]
"agent.max_output_bytes" = 51200
```

`force` uses the ordinary configuration schema and has highest precedence.
Supported ceilings are `agent.max_output_bytes`,
`agent.compaction.reserve_tokens` and `agent.compaction.keep_recent_tokens`;
explicit or resolved values above them fail rather than silently changing.
Allowlist entries match exact hosts, command paths or environment variable
names. Omitted lists impose no constraint; empty lists deny all. Command
allowlists constrain the executable, not its arguments. Credential-variable
policies also deny MCP OAuth. `require_sandbox` constrains bash, prohibits
excluded commands and elevated execution, and cannot provide confinement on
Linux. MCP stdio remains a separate external process governed by MCP policy.
Hooks and custom commands follow sandbox and executable trust boundaries; see [Hooks and custom tools](memory-and-extensions.md#hooks-and-custom-tools).

A complete user configuration example:

```toml
default_profile = "deepseek"

[ui]
mode = "auto"

[agent]
shell_timeout = "120s"
max_output_bytes = 51200
# End-to-end deadlines are disabled when omitted, except the unattended one.
# turn_timeout = "30m"
# unattended_turn_timeout = "30m"  # chat and --prompt turns; "0" disables
# provider_timeout = "10m"
# subagent_timeout = "1h"
# workflow_step_timeout = "1h"
cancellation_grace = "5s"
# Limits that stop a turn which makes no progress. 0 disables one condition.
# no_progress_repeats = 8
# no_progress_failures = 12

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
# session_idle_timeout = "30m"  # close a session this long after its last use
# max_open_sessions = 64  # keep at most this many sessions open

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
# Optional deprecated override; catalog discovery needs no manual list:
# subagent_models = ["smaller-model-id", "gpt-5.6"]
```

Key points:

- `default_profile` names the profile used when no `--profile` is given and no
  session supplies one.
- Sub-agent selection works by default without a manual model list. For both
  `chatgpt` and `openai-compatible`, the parent calls `list_models` before
  ordinary delegation, unless you explicitly chose a model, a named agent has a
  model default, or the legacy override below applies. Discovery uses the active
  connection's authentication: `otto login` for ChatGPT, or the profile's API-key
  environment variable for an OpenAI-compatible endpoint. No extra credentials
  or discovery setup are required. The parent reuses the catalog within the
  current connection and refreshes after a connection change; a full catalog is
  not injected into every request. It selects an economical adequate model via
  `agent.model` and puts a brief reason in the existing task description. Routine
  searches, extraction, formatting and checks favor cheaper adequate models;
  demanding reasoning can justify stronger ones. Suitability is the parent
  model's judgment, not a built-in price ranking or runtime classifier. Metadata
  is only available where the provider supplies it; missing prices or capability
  details are not inferred as facts. If discovery fails, returns no models, or
  suitability is unknown, the parent uses the session model instead of guessing
  IDs or blindly retrying. See [discovery and recovery](safety-and-tools.md#list_models).
- `[profiles.<name>].subagent_models` is an optional deprecated compatibility
  override in user configuration, not a project's `.otto/config.toml`. It lists
  up to 16 exact model IDs in your preferred lower-cost-to-higher-capability
  order, taking precedence over catalog selection. The parent chooses the first
  adequate candidate, while explicit user choices and named-agent defaults
  still take precedence. Otto has not verified this order's prices or availability.
  An `agent` call that omits `model` uses the first entry, after
  any named agent's default; without a list, it inherits the session model.
  The child uses the same provider connection as the parent. A provider,
  endpoint or credential-source override that changes that connection disables
  the list for the session. Otto reports a diagnostic when a connection change
  disables configured candidates; select the intended profile or remove the
  conflicting override, then restart. An unavailable ID fails the child with the
  provider's error; correct the ID or choose another available model. Inspect
  partial work and possible side effects before retrying: Otto does not
  automatically repeat the task. Restart Otto after editing the list to load
  the new configuration; already-recorded tasks retain their selected model.
  On upgrade, existing lists keep these semantics; remove the override to use
  catalog-first selection. Install the updated binary with `make install` and
  restart Otto (including a running `otto serve`) to load the new policy and
  tool registration. Existing task records and their model provenance remain
  readable and are not rewritten.
- `[ui].mode` sets the frontend mode (`auto`, `tui`, `repl`).
- `[agent].shell_timeout` and `[agent].max_output_bytes` set default limits.
- `[agent]` also accepts optional end-to-end `turn_timeout`,
  `provider_timeout`, `subagent_timeout`, and `workflow_step_timeout` values.
  Omitted deadlines are unlimited for backward compatibility. A child always
  inherits the earlier of its own configured deadline and its parent's
  remaining deadline. The turn deadline covers one prompt step: compaction and
  provider retries inside the step never reset it, while each task-queue
  follow-up and each retry after an approved command starts a new budget, and
  time spent waiting for an approval decision is not counted.
- `[agent].unattended_turn_timeout` (default `30m`) is the turn deadline for
  unattended turns: turns from a Telegram or Feishu chat (through `otto acp`,
  directly or relayed to `otto serve`) and every turn of an `otto --prompt`
  run. Such a turn uses the earlier of `turn_timeout` and
  `unattended_turn_timeout`. `"0"` disables it. TUI, REPL, web UI and editor
  ACP turns are not affected; scheduled prompt runs keep their own 10-minute
  limit. When the deadline expires the step ends with
  `provider call deadline exceeded`; the session keeps its history and the
  next prompt continues it.
- `[agent].cancellation_grace` defaults to `5s` and bounds cooperative
  cleanup before Bash or a shared stdio MCP server is force-stopped.
- `[agent].no_progress_repeats` (default `8`) and
  `[agent].no_progress_failures` (default `12`) stop a turn that makes no
  progress, for every turn including each sub-agent's. `0` disables one
  condition and values must be between `0` and `1000`. The repetition limit
  also counts unchanged failed cycles of 2–4 calls. See
  [Stopping a turn that makes no progress](safety-and-tools.md#stopping-a-turn-that-makes-no-progress).
- ChatGPT connection/send/body failures, HTTP 429/500/502/503/504 responses,
  a clean stream close before its terminal event, and stream errors classified
  as `server_is_overloaded` or `server_error` are retried up to six times,
  including interruptions after partial output, waiting 1, 2, 4, 8, 16, then
  30 seconds within the same turn deadline. A numeric `Retry-After` header takes
  precedence and is capped at 30 seconds. Only the current model request is
  repeated; previously executed tools are not rerun. The TUI shows the retry
  countdown and the REPL prints each planned retry. Interrupted partial output
  is marked before the replacement reply in the TUI, REPL and ACP clients;
  only the completed reply is saved as assistant history. Parent and sub-agent
  requests use the same recovery policy. Other HTTP statuses, other stream
  error codes (such as `rate_limit_exceeded`), authorization errors, and
  malformed or oversized stream events are not retried. Cancellation stops
  recovery immediately. After the retry limit or deadline, the turn ends with
  an error and the interactive session remains available. Normal model steps
  record attempt counts and safe retry reasons in `otto.provider_recovery`
  session entries when the request settles.

  A Responses stream accepts error classification in `response.error`, a
  nested event `error`, or top-level fields, and falls back to the SSE `event:`
  name when the JSON payload omits `type`. One event payload is limited to
  1 MiB; an SSE line may hold one whole event at that limit.

  An OpenAI-compatible request is tried again up to three times, 1, 2, then
  4 seconds apart within the same turn deadline, in two cases. When the
  connection cannot be established (refused, unreachable, or a failed TLS or
  connect timeout), the request never left Otto and is reported as not
  started if all attempts fail. When the request was sent but its transport
  failed or its stream timed out before any text, reasoning or tool-call delta
  reached Otto, the endpoint may have started the completion, so the outcome
  is reported as unknown. A failure after a delta, every HTTP status and every
  malformed stream event are not retried.

  Each response stream must produce a text, reasoning or tool-call delta at
  least every 120 seconds on ChatGPT and every 300 seconds on an
  OpenAI-compatible endpoint, counted from the response headers. Keepalive
  bytes and SSE comments do not count. When the interval passes, the attempt
  ends with retry reason `model output idle timeout` and follows the rules
  above. A response that sends no bytes for 60 seconds ends with
  `stream read timeout` on ChatGPT. These intervals are fixed and cannot be
  configured.

Deadline checks are cooperative at synchronous filesystem and Turso
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
  [platform support](getting-started.md#platform-support).

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
  without an escalation request. A matched command in a
  [classified class](safety-and-tools.md#classified-commands-wait-for-approval-inside-the-sandbox),
  such as `gh pr create` or `rm -r`, still waits for that approval first. An
  excluded program has your full user access:
  list only programs you trust with that, such as `lark-cli`, which keeps its
  configuration in `~/.lark-cli` and its secret in the keychain.

  The list is read only from the config file Otto loads
  (`~/.config/otto/config.toml` or `--config`); a workspace cannot add entries.
  It applies to the parent session, child agents, and `otto --prompt` runs
  whenever the sandbox mode is Seatbelt. With `--sandbox off`
  every command is already unconfined. If the unconfined environment cannot be
  built with complete redactions, the list has no effect and commands stay
  sandboxed. `/sandbox exclude` and `/approve <id> always` (which adds the
  exact approved command) add entries from a running session.

  `/sandbox reload` applies edits to `network`, `read_paths`, and
  `excluded_commands` to the running process (see
  [Slash commands](interactive-use.md#slash-commands)). Three changes still need a restart:
  `allow_env`, because the shell environment is fixed when the `bash` tool is
  built; `driver`, because switching between Seatbelt and `off` changes that
  environment; and any change made when the sandbox was already unavailable at
  startup, because there is no `bash` tool to re-point.

- `[skills]` discovers reusable instruction sets from configured roots and
  registers the `skill` tool when at least one skill is found. Config keys are
  `enabled` (default true) and `paths` (default `["~/.otto/skills", ".otto/skills"]`);
  TOML-only, no CLI flags or environment variables.
- `[agent.compaction]` configures automatic context compaction (see
  [Context compaction](interactive-use.md#context-compaction)).
- `[server].socket` sets the Unix domain socket path for `otto serve`, and
  `[server].listen` a loopback `HOST:PORT` to use instead (see
  [Agent server](servers-and-connectors.md#agent-server)). TOML-only aside from the `--socket` and
  `--listen` flags; no environment variable.
- `[server].workspace_roots` lists directories under which `otto serve` may
  load another workspace besides the startup one (see
  [Workspaces](servers-and-connectors.md#workspaces)). Default empty: only the startup workspace is
  admitted. TOML-only, no CLI flag or environment variable.
- `[server].session_idle_timeout` is how long `otto serve` keeps a session
  that has nothing running, as a duration (`"30m"`, `"2h"`); the default is
  `"30m"`, and `"0"` keeps idle sessions open until a client closes them.
  `[server].max_open_sessions` is how many sessions one process keeps open,
  closing the least recently used idle ones above it; the default is `64`, and
  `0` keeps every session open. Both take effect without a restart and are on
  by default; see [Session lifetime](servers-and-connectors.md#session-lifetime). TOML-only, no CLI flag
  or environment variable.
- `[projects."<path>"]` records a trusted directory: `otto serve` admits it
  and its descendants as workspaces (see [Workspaces](servers-and-connectors.md#workspaces)).
  `trust_level` is required and `"trusted"` is its only accepted value; any
  other value fails config loading. `otto trust <dir>` appends the table as
  text, so the rest of the file is kept byte for byte; there is no environment variable
  and no HTTP route that adds one.
- There is no `[inbound]` table. A config that still has `[inbound.feishu]`
  fails to load with an error that says to delete the table. Feishu and
  Telegram messages reach otto through the
  [chat connector](servers-and-connectors.md#chat-connector).
- `[failover]` decides whether a session gets a lease directory so it can be
  continued on another host (see
  [Continuing a session on another host](interactive-use.md#continuing-a-session-on-another-host)).
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
  [ChatGPT subscription](getting-started.md#chatgpt-subscription).
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

A resumed session always receives the tools of the current configuration. When
that tool set differs from the one recorded for the session, or the session has
history and was written before Otto recorded tool sets, Otto appends one
`capability_update` message before the run's user message. It is shown in the
transcript and stays in the session history, so every later request contains
it. It lists the added and removed tool names and the names of tools whose
description or parameters changed. When the Otto version differs from the
recorded one, the message also states both versions, and when `otto_help` is
available it states that earlier `otto_help` results labelled with the old
version describe the previous version. When the earlier tool set was not
recorded (sessions written by older Otto versions, and sub-agents started with
`context: inherit`), it lists the current tool names instead. A new session
gets no such message.

Otto stores, in an `otto.capabilities` session entry, the tool names, one
digest per tool of its description and parameters, and the Otto package
version. A field missing from an entry written by an older Otto is unknown and
produces no line. A version change with no tool change and no `otto_help`
produces no message. The message lists names, not content. Changes to the
manual sections, the system prompt policy text, workspace instructions, and the
skill and agent catalogs are not reported individually.

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

`otto config set` and commands that write the user config file — `otto mcp add`/`remove`/`enable`/
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

---

Previous: — · [Documentation index](../README.md) · Next: [Interactive use](interactive-use.md)
