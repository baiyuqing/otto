# Layered configuration and project trust

Status: approved 2026-10-05 and implemented in this change. Current user
behavior is documented in the [user manual](../manual/getting-started.md#configuration).

## Problem and existing behavior

Otto auto-discovers only `~/.config/otto/config.toml`; `--config` selects one
explicit file. The core owns pure TOML parsing and runtime resolution, while
the native crate owns files, environment lookup, and backed-up writes.
Deserializing directly into `File` supplies defaults before any merge, which
loses presence information for many strings, integers, and arrays.

`otto trust` currently records directory admission for `otto serve`. It is not
an authorization grant for project-provided executables. Skills already merge
user and workspace roots by name, with workspace definitions winning. Prompt
context currently loads one workspace-root instruction file, preferring
`AGENTS.md` to `CLAUDE.md`. These paths need explicit trust and scope rules
alongside the new configuration loader.

## Layers and discovery

Ordinary values have this precedence, from highest to lowest:

1. Managed organization policy.
2. Workspace-local configuration.
3. Workspace-shared configuration.
4. User-global configuration.
5. Built-in defaults.

Paths:

| Layer | Location | Discovery |
| --- | --- | --- |
| Managed | `/etc/otto/policy.toml` on Linux; `/Library/Application Support/Otto/policy.toml` on macOS | Fixed administrator-controlled path, never a project or ordinary CLI override |
| User | `~/.config/otto/config.toml` | Existing default; `--config PATH` replaces this source only |
| Shared | `<directory>/.otto/config.toml` | Root-to-workspace ancestor chain within a fixed project boundary |
| Local | `<directory>/.otto/config.local.toml` | Same chain; users add these paths to their repository ignore rules |

The discovery boundary is the enclosing Git worktree root when present;
otherwise it is the explicitly selected workspace directory. Never walk above
that boundary. A nested Git checkout establishes its own boundary. Canonical
paths and pinned file opens prevent symlink escapes; a project config may not
resolve outside its owning directory boundary. Discovery does not run Git
hooks or require the bash tool.

Merge all shared sources root-to-leaf, then all local sources root-to-leaf.
Thus a local root setting beats a deeper shared setting; deeper settings win
within the same class. This is intentionally different from alternating local
and shared files at each directory. Record that ordering in diagnostics.

Explicit CLI/session selections form a request overlay above ordinary file
values, preserving current flag behavior. They remain subject to field-source
restrictions and managed constraints. `--config`, `--sandbox off`, ACP requests,
and session commands cannot bypass managed restrictions.

## Merge contract

Merge presence-preserving raw TOML documents before applying schema defaults
or resolving environment references. Attach source and layer metadata to every
leaf. Then validate allowed fields, merge, enforce managed constraints, and
resolve into the existing typed runtime structures.

| Kind | Rule |
| --- | --- |
| Scalar | Highest eligible source wins; explicit `false`, `0`, and empty strings remain explicit and undergo existing field validation |
| Object/table | Recursive merge by field, not replacement of the entire table |
| List | Complete replacement by the highest source that contains the key; `[]` clears it |
| Named map | Union by name, then merge fields of the same entry, subject to the executable-identity and credential rules below |

Missing keys inherit. TOML has no `null`, so there is no implicit deletion
marker. For named entries already supporting `enabled`, use `enabled = false`
to disable them; do not invent deletion or `+item`/`-item` syntax in this change.
There is no profile tombstone: select another profile or remove a definition
from its owning file. Invalid explicit values fail rather than falling back.
Unknown keys retain the current rejection behavior, including its existing
experimental-table exception.

Malformed discovered project configuration fails with its file and line;
forbidden but syntactically valid project fields are ignored with diagnostics.
An untrusted project's inactive executable settings are still shown as
inactive, not merged into runnable definitions.

## Field policy

Both project layers have the same authority restrictions. An ignored local
file is still project-controlled data and is not a credential authority.

| Fields | Eligible sources and rules |
| --- | --- |
| `default_profile`, profile `model`, `thinking`, context/compaction windows, UI mode, non-security agent limits and compaction | All ordinary layers; managed constraints always apply |
| Profile `provider`, `base_url`, `api_key_env` | User or managed only; project definitions can customize a profile's model settings but cannot provide or redirect its transport/authentication |
| `sandbox.*`, server listeners and workspace roots, `projects.*`, failover/storage paths, memory backend/path, skill/agent search roots, reflection write destinations and automatic persistent writes | User or managed only; project keys ignored even after trust |
| MCP `enabled` and timeouts | All layers; disabling cannot override a managed required capability, if policy declares one |
| MCP `transport`, `command`, `args`, `cwd`, `url` and project skill/agent definitions | Project sources require executable-config trust; managed policy can prohibit them |
| MCP `env`, `headers`, authentication/OAuth settings, environment forwarding and secret references | User or managed only; no project-supplied credential values or variable references |
| Experimental fields | Project sources ignored until individually classified; no permissive unknown-field path may enable an executable capability |
| Legacy ignored fields such as `web.allow_hosts` | Preserve compatibility and clearly report that they have no effect |

Audit every field before implementation and reject unclassified project
fields by default. Secret values remain environment bindings or existing
OAuth credential stores; no layer introduces plaintext credentials in TOML.

Named MCP definitions must not accidentally combine a user credential with a
project-selected executable or destination. Treat execution identity
(`transport`, command/arguments/working-directory, or canonical HTTP URL) as
one atomic unit. If a project overrides that unit for a user/managed server
with credential bindings, ignore the override and report it. A project may
introduce a distinct trusted server name without inheriting other entries'
credentials. Changing transport drops incompatible transport-specific fields;
it must never preserve hidden auth from the old transport. Managed restrictions
match canonical destinations/executable identity, not only server names, so
renaming a prohibited server cannot evade policy.

## Managed policy

Managed policy is a constraint document, not simply one more ordinary TOML
table. It can force values, forbid capabilities/destinations, and bound numeric
limits. Apply it after file/CLI/session resolution, and at runtime capability
boundaries. A forbidden request fails with a policy diagnostic; silently
changing an explicitly requested security capability is not acceptable.

Validate administrator ownership and non-user-writable ancestor directories
for managed files and referenced policy artifacts. Missing policy is permitted
on unmanaged machines; an existing invalid/unreadable policy fails closed.
Policy files cannot be supplied through project paths or user environment
overrides. The OS/managed launcher supplies the protection boundary: this
guarantee applies to this Otto process and its supported interfaces, not to a
user who can replace the binary, alter the OS, or run an unrelated tool.

The initial policy schema should cover the capabilities Otto actually has:
sandbox requirements, allowed provider/MCP destinations, allowed credential
bindings, project-executable enablement, and resource ceilings (output bytes and compaction reserve/keep tokens).
`agent.max_turns` is a legacy unused field and is not a supported ceiling.
Do not add a
general hooks/extensions framework or claim unsupported providers.

## Project trust

Keep workspace admission separate from executable configuration approval.
Existing trusted-directory records continue to admit workspaces but do not
silently grant the new executable-config authority.

Extend `otto trust <directory>` to display the discovered executable sources
and require an explicit `--executables` grant to activate them. Noninteractive
invocations require that explicit option; no unanswered prompt grants trust.
Persist approval in user-controlled state keyed by canonical project root and
a digest of executable config/skill/agent contents and source paths. Moving or
changing executable definitions invalidates the grant. Ordinary UI/model
setting changes do not invalidate it. New/replaced symlinks are rechecked.
Expose a revoke operation and apply revocation to future tool calls as well
as new sessions; running child processes must be stopped where the runtime
supports cancellation. Revocation must not be represented as retroactively
undoing external side effects.

Untrusted projects may supply non-executable preferences and instruction text.
Their MCP processes, project skills, and project agent definitions remain
inactive, with a visible reason. Reading a file manually does not grant
permission to run a capability it describes. User-global capabilities remain
available under their existing authority and managed restrictions.

## Instructions and prompts

Instructions are appended in explicit groups; no project file replaces a
global or managed instruction source. Otto's built-in runtime rules remain
system-owned and cannot be removed by configuration.

Load optional user instructions from `~/.config/otto/AGENTS.md`, then project
instructions root-to-workspace using the same boundary as config discovery.
At each directory, load `AGENTS.md`, or `CLAUDE.md` only when `AGENTS.md` is
absent. More specific directories refine broader project guidance, while
managed/system rules and direct user instructions retain their authority.
This change only applies the initial workspace chain; dynamically loading
instructions when file tools enter arbitrary descendant directories is a
separate behavior and is not implied here.

Each file stays in a source-labelled escaped block, with secret redaction and
an 16 KiB per-file bound plus a 64 KiB total instruction budget. Excess input
fails with the offending source rather than silently dropping the highest
authority instructions. The total budget covers user/project instruction
files, not Otto's required system policy. Project text is not an executable
trust grant and cannot alter sandbox, credentials, or MCP policy.

## Explain, editing, and lifecycle

Add `otto config explain --cwd PATH` (and JSON output) to show discovered
files, final non-secret values, source provenance, overwritten values, ignored
fields, inactive capabilities, and managed constraints. It does not start
MCP, contact providers, or resolve/display secret values. Diagnostics never
print credential-bearing environment values, headers, or raw credential files.

Existing writers continue to write the explicit user config or the default
global file. They must not serialize an effective merged config back into one
source. Any new project-edit command must specify `--scope shared|local`, show
its destination, and use the existing backup/CAS writer contract. Read-only
layers never receive writes. Explain warns when a write is shadowed by another
layer. Generating a local file does not silently modify `.gitignore`.

Load and validate per workspace, including additional workspaces admitted by
`otto serve`. Process listeners, global credential stores, and managed policy
belong to the server process; workspace preferences and trusted executable
catalogs belong to each workspace runtime. Attach/ACP clients use the server's
effective config and cannot inject untrusted client paths as config sources.

Snapshot ordinary configuration when a workspace runtime is created. Changes
take effect after restart or an existing explicit reload operation; do not
promise a generic watcher. Sandbox reload re-applies managed constraints and
source restrictions. Trust revocation is an authorization change and is
checked immediately for future operations, independently of that snapshot.

## Compatibility, implementation, and acceptance

No project files means existing global configuration and CLI behavior remain,
unless a managed policy constrains them. Existing project admission is
preserved. Activating executable project configuration requires a new explicit
trust grant. Existing automatically discovered project skills/agents become
inactive until trusted; document that migration as a compatibility change.

Implement in this order, with reviewable commits in one development branch:

1. Pure core merge/source rules and field classification; native discovery,
   provenance, and explain command.
2. Managed constraints and trust records, capability checks, and revocation.
3. CLI/server/ACP integration, safe editing, instruction chain, skill/agent
   discovery, and migration documentation.

Acceptance tests must cover absence versus explicit false/zero/empty values;
list replacement/clearing; named-map union/override/disable; monorepo ordering;
symlink and nested-checkout boundaries; forbidden project fields; global
credential isolation from project MCP/profile redirects; policy enforcement
through flags, reload, and attach; inactive untrusted capabilities; trust
invalidation/revocation; instruction ordering/fallback/budgets; per-workspace
server isolation; writer destinations; and redacted explain output.

Use offline local fixtures for functional checks, preserve the wasm-safe core
boundary, and run focused tests plus `make check-fast`. macOS CI validates
Seatbelt; Linux results do not establish macOS sandbox readiness.
