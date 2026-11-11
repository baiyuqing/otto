# Getting started

[Documentation index](../README.md) · Next: [Interactive use](interactive-use.md)

## Prerequisites

Install the toolchains pinned in `rust-toolchain.toml` and
`chat-connector/go.mod` (Rust and Go), then configure the first-party
**Claude API**, an **OpenAI-compatible** endpoint, or the **ChatGPT** provider.
Model requests leave your machine; sessions, configuration and default memory
remain local.

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
stopping every Otto process (including `otto serve` and connectors), rerunning
`make install`, and starting them again. Old and new binaries must not use the
same `~/.otto/` databases at the same time; the [changelog](../../CHANGELOG.md)
lists breaking changes that need extra steps.
Browser UI development/hosting is separate: [otto-plus](https://github.com/baiyuqing/otto-plus).

For Claude or an OpenAI-compatible connection, keep the API key in the
environment, not TOML, a command-line argument, or a prompt:

```bash
export ANTHROPIC_API_KEY='…'  # set privately; avoid saving a real key in shell history
otto --model claude/MODEL_ID

export OPENAI_API_KEY='…'
otto --model openai-compatible/MODEL_ID --base-url https://llm.example.com/v1
```

The built-in `claude` provider defaults to `https://api.anthropic.com`, API version
`2026-10-21`, and the `ANTHROPIC_API_KEY` binding. If the key is not scoped to
one Claude workspace, set a separate environment variable containing the
workspace ID and name it with the provider's `workspace_id_env`; do not put the
workspace ID or key in TOML. `api_version` can pin a different Claude API
version when required. The Claude provider covers the Messages API used by
Otto; Bedrock, Vertex AI, Foundry, OAuth and workload identity are different
connections and are not supported by this provider.

Claude prompt caching is enabled by default: Otto sets cache breakpoints on the
system text (which also covers the tool definitions), on the end of the
conversation before the last two user messages, and on the last message block.
It uses the provider's five-minute lifetime; active hits refresh that lifetime.
The first write and a write after expiry use the provider's cache-write price.
Prefix changes, small prompts and endpoint/model support affect actual hits.
A custom Claude endpoint must accept the documented `cache_control` fields;
Otto surfaces a rejection instead of silently retrying without caching.
See [Anthropic's caching contract](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).

Use a model ID supported by your provider. Each OpenAI-compatible request
carries `prompt_cache_key` set to the session id (a sub-agent uses its own
transcript id), so an endpoint that routes by this field can serve a session's
requests from the same prompt cache. If the endpoint answers HTTP 400 or 422 with an
error body that names `prompt_cache_key`, Otto resends that request once
without the field and omits it from later requests until the provider client
is rebuilt, for example when Otto restarts. `otto setup` can create a provider
and model entry; [Providers and models](#providers-and-models) describes the
configuration format. `otto --help` and each subcommand's `--help` are the complete CLI
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
API-key variable that Compose forwards, or add the provider's selected variable
to the service `environment` mapping. A default Claude provider therefore needs
`ANTHROPIC_API_KEY` added there; add `ANTHROPIC_WORKSPACE_ID` too when
`workspace_id_env` names it. Export values before `compose up`; they are not
build arguments. Use `host.docker.internal`, not container `localhost`, for a
model endpoint on the Mac. The published API is Mac-loopback only, retains
bearer authentication, and gets a new token after each server restart.

Compose does not publish ChatGPT OAuth callback ports. Sign in natively, select
a ChatGPT provider in container setup, and import private login state into its
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
logs, prompts or examples. Otto refreshes an expired token and retries once when
ChatGPT rejects the token; if authorization still fails, run `otto login` in the
runtime's environment and restart it. Login, logout and token refresh serialize
credential changes so an in-flight refresh cannot restore a signed-out account
or overwrite a newer login. See [Troubleshooting](troubleshooting.md).

Changing the signed-in account still requires restarting running Otto processes.
A process that already captured the previous account does not switch accounts at
runtime.

## Command-line reference

| Entry point | Purpose |
| --- | --- |
| `otto setup` | Guided provider and model setup |
| `otto --cwd DIR` | Start in a selected workspace |
| `otto --model PROVIDER/MODEL_ID` | Select the model for this run ([selectors](#providers-and-models)) |
| `otto --base-url URL` | Override the selected provider's endpoint |
| `otto --prompt TEXT` | One headless turn; combine with `--no-session` if persistence is unwanted |
| `otto --continue` / `--resume PATH` | Reopen history for the current workspace |
| `otto --archive PATH` | Archive one active session and exit; recoverable and does not reclaim disk space |
| `otto delete <SESSION>` | Permanently delete by ID or exact name; prompts for confirmation and shows the ID (`--force` requires an ID); cannot be undone |
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

Use the environment binding selected by the active provider
(`api_key_env` for Claude and OpenAI-compatible providers, plus optional
`workspace_id_env` for Claude). MCP, chat and plugin package credentials
likewise come from their configured environment bindings, never literal
secrets in configuration. `otto login` manages ChatGPT credentials.

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
Providers merge by name, then model ID, then field; MCP servers merge by name then field. There is no `+item`, `-item`,
`null` or deletion syntax. Use `enabled = false` where supported. Changing an
MCP transport removes its former transport's fields.

Projects may set `default_model`, UI mode, agent limits/deadlines and the
`thinking`, `context_window` and `compaction_window` fields of a model entry
of a built-in or host-defined provider. A project cannot define a provider or
set a provider-level field. Provider type/URL, credentials,
sandbox grants, listeners, admission, storage and catalog roots
require user/managed sources. Ignored fields produce diagnostics. Projects
cannot retarget credential-bound MCP servers, interpolate environment variables
in project MCP definitions, or supply their env, headers or OAuth bindings.
Plugin package bindings (`[plugins.bindings.*]`) are also host-only.

### Inspecting and editing scopes

```bash
otto config explain --cwd ./project --json
otto config set ui.mode '"repl"' --scope local --cwd ./project
otto config set 'providers.chatgpt.models."gpt-5.6-luna".thinking' '"high"' --scope shared --cwd ./project
otto config set default_model '"chatgpt/gpt-5.6-luna"' --scope user
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

The terminal's sub-agent observer shortcuts are configured under
`[ui.observer]`; see [observer keys and examples](interactive-use.md#sub-agent-observer).
They are client-local user/managed preferences, also when using `--attach`.

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

A small user configuration illustrates the common settings. A longer,
commented sample with ChatGPT, Claude and custom OpenAI-compatible providers
is [examples/config.example.toml](../../examples/config.example.toml).


```toml
default_model = "claude/MODEL_ID"

[ui]
mode = "tui"

[providers.claude]
type = "claude"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
api_version = "2026-10-21"
# workspace_id_env = "ANTHROPIC_WORKSPACE_ID"  # only when the key requires it
models."MODEL_ID" = { thinking = "high" }

[sandbox]
driver = "auto"                # "auto" selects Seatbelt on macOS
network = "allow"              # "allow" or "deny" IP networking
deny_read_paths = []           # added to the built-in read denylist
read_paths = []                # exceptions: readable although the denylist covers them
allow_env = []                 # exact environment variable names restored in the sandbox

[agent]
shell_timeout = "120s"
max_output_bytes = 51200
# Optional deadlines: turn_timeout, provider_timeout, subagent_timeout,
# workflow_step_timeout. The unattended deadline defaults to 30m.
# unattended_turn_timeout = "0"  # explicitly disable it

[agent.context]
strategy = "compaction" # "compaction" (default) or "view-tree"

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

### Providers and models

A provider is one endpoint and credential binding of one API type. A model is
one model of a provider, written as one line in the provider's table. A model
is selected by `<provider>/<model_id>`, split at the first `/`, so the model
ID may contain `/`. Otto has no model aliases.

```toml
default_model = "chatgpt/gpt-5.6-luna"

[providers.chatgpt]
type = "chatgpt"
models."gpt-5.6-luna" = {}
models."gpt-5.6-terra" = {}
models."gpt-5.6-sol" = {}

[providers.gateway]
type = "openai-compatible"
base_url = "https://llm.example.com/v1"
trusted_origins = ["https://llm.example.com"]
api_key_env = "LLM_GATEWAY_API_KEY"
models."model-a" = { thinking = "high", context_window = 128000, compaction_window = 100000 }
```

`[providers.<name>]` fields:

| Field | Meaning |
| --- | --- |
| `type` | `claude`, `openai-compatible` or `chatgpt`. Required unless the name is a built-in provider. |
| `base_url` | Endpoint. Required for `openai-compatible`; Claude default `https://api.anthropic.com`; rejected for `chatgpt`. |
| `trusted_origins` | Independent user/managed grants for custom endpoint origins; no paths. See [endpoint trust](#endpoint-trust). |
| `allow_http_origins` | Additional explicit plaintext HTTP acknowledgement for each granted HTTP origin. |
| `api_key_env` | Environment variable holding the key; Claude default `ANTHROPIC_API_KEY`; rejected for `chatgpt`. |
| `api_version` | Claude only. |
| `workspace_id_env` | Claude only; environment variable holding the workspace ID. |

`models."<model_id>" = { ... }` fields: `thinking` (`low`, `medium`, `high`,
`xhigh` or `max`), `context_window`, and `compaction_window`, which requires
`context_window`. The model ID is quoted. `models."<model_id>" = {}` lists the
model in the `/model` picker and uses built-in limits. A model may also be
written as its own table, `[providers.<name>.models."<model_id>"]`; both forms
produce the same configuration. `otto setup`, `otto config set` and
`POST /v1/config/changes` write a new model as a `models."<id>"` line inside
`[providers.<name>]`, create that table when it is absent, and edit or remove
an existing model in the form it is written in. A provider whose models are
all written as `[providers.<name>.models."<id>"]` tables receives a new model
in that form, so one provider never holds both forms.

A provider name uses ASCII letters, digits, `_` and `-`. Three providers are
built in and named after the API types: `claude`, `openai-compatible` and
`chatgpt`. A table with a built-in name adds models to that provider or
overrides its fields; `type` may be omitted and, when present, must equal the
name. A selector may name a model ID that has no model table, under any
configured or built-in provider; built-in limits and defaults then apply. An
unknown provider name is an error.

| Selector input | Meaning |
| --- | --- |
| `default_model = "P/ID"` | Default model. |
| `--model P/ID` | Model for this run. |
| `OTTO_MODEL=P/ID` | Model for processes started with this variable. |
| `approval.reviewer_model = "P/ID"` | Model of the approval reviewer. |
| `--provider P --model ID`, `OTTO_PROVIDER=P` with `OTTO_MODEL=ID` | Deprecated; same as `P/ID`. |
| `--model ID` without `--provider` | Error; Otto does not take the provider from `default_model`. |

### Precedence

The model is chosen in this order: `--model`, `OTTO_MODEL`, the resumed
session's model (only when neither of the two selected one), then
`default_model`. The deprecated `--profile` ranks with `--model` and the
deprecated `OTTO_PROFILE` ranks after the resumed session. `--provider` without
`--model` is an error. A session recorded before selectors existed stores a
provider type and model ID; it resumes on the provider of `default_model` when
that provider has the same type, otherwise on the built-in provider of that
type. `--base-url` overrides the endpoint of the selected provider. Other
stored values follow the source rules above. Select a model by its full
selector rather than mixing provider, endpoint and credential overrides.

### Migrating from `[profiles]`

Files that use `[profiles.<name>]`, `default_profile`,
`approval.reviewer_profile`, `--profile` or `OTTO_PROFILE` still load. Otto
converts each profile to a model of the built-in provider of its type, or, when
its connection fields differ from the built-in defaults, to a provider shared by
profiles with identical connection fields. Each converted file produces a
startup diagnostic, also shown by `otto config explain`, that prints the
equivalent new TOML. A file that mixes legacy and new tables is an error, as are
two profiles that convert to the same selector with different settings. Otto
does not rewrite the file.

Use the equivalent TOML from `otto config explain` when updating an older
file, and follow the current [provider/model format](#providers-and-models)
for new configuration.

A managed policy that forces `default_profile` or `profiles.*` fails to load
with an error naming the replacement keys (`default_model`,
`providers.<name>.<field>`, `providers.<name>.models."<id>".thinking`).
Session history, usage records and workflows written earlier keep their stored
`profile` field names and load without migration.

Subagents normally use authenticated `list_models` discovery, falling back to
the session model if discovery is unavailable or suitability unknown. Explicit
user model choices and named-agent defaults take precedence. The optional,
deprecated `subagent_models` list of a converted legacy profile overrides
catalog selection; it is not required and is disabled by a change to its
provider connection. See [`list_models`](safety-and-tools.md#list_models).

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

### Endpoint trust

Before reading provider credentials or sending a request, Otto independently
checks the selected endpoint against user/managed provider origin grants.
The built-in HTTPS origins `api.anthropic.com` (Claude), `api.openai.com`
(OpenAI-compatible), and `chatgpt.com` (ChatGPT) are trusted for their respective
types. An origin is the scheme, normalized host and effective port, not a path.
Lookalike domains, different ports, and HTTP are not official origins.

Custom endpoints **fail closed**, including existing configurations after an
upgrade, `--base-url` overrides, setup input, model catalogs and background agents.
Setup does not silently trust a URL. For a custom endpoint, manually add an
explicit grant to the matching provider table in your user configuration
(`~/.config/otto/config.toml`, or the trusted user file selected by `--config`), or ask
your administrator to add it in managed configuration:

```toml
[providers.gateway]
type = "openai-compatible"
base_url = "https://llm.example.com/v1"
api_key_env = "GATEWAY_API_KEY"
trusted_origins = ["https://llm.example.com"]
```

Keep your existing model entries and selected model. Grants contain origins
only: no path beyond `/`, userinfo, query, fragment, or secrets. A custom local
endpoint needs its own exact-origin grant. Plaintext HTTP additionally requires
a separate acknowledgement of unencrypted prompts and credentials:

```toml
trusted_origins = ["http://127.0.0.1:8080"]
allow_http_origins = ["http://127.0.0.1:8080"]
```

Put those fields in the relevant provider table, not a model or project table.
They do not follow `--base-url`: a changed origin needs a new independent grant.
Metadata and link-local literal addresses are forbidden even with grants.
Redirects are never followed; configure the final endpoint directly. Restart
running Otto processes after editing configuration. To revoke access, remove
the origin grant and restart. No credential value belongs in these files.

A grant is **not a safety certification**: arbitrary HTTPS hosts can still be
malicious, and trusted hosts/DNS can change. Grant only services you control or
have independently verified. User/managed configuration is trusted authority;
an agent with approved unrestricted shell writes to that config could change
it. Existing shell approval and filesystem confinement are the protection, not
a claim that a user config file is tamper-proof. No model-callable trust grant
or provider-mediated approval is offered.

Library integrations should use `TrustedEndpoint::authorize` with policy from
trusted application authority and each provider's `Client::new_trusted`.
Legacy constructors allow official origins only. Injected `reqwest::Client`
constructors fail closed because their redirect policy cannot be inspected;
migrate to the owned, no-redirect transport. Library callers supplying policy
are themselves trusted; never derive grants from model output or a URL override.

Provider transports check every DNS answer before connecting. A result set with
any metadata/link-local address is rejected in full, including mapped IPv4 and
well-known NAT64 forms; DNS cannot silently fall back to another resolver.
Private/local addresses remain usable only behind the independent origin grant.
Owned provider completion and catalog clients disable environment/system proxies
(including `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`) because a proxy could
resolve the destination outside this policy. Transparent OS/network routing is
outside Otto's resolver boundary; grants are still not service certification.
