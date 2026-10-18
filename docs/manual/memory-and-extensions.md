# Memory and extensions

Previous: [Automation](automation.md) · [Documentation index](../README.md) · Next: [Troubleshooting](troubleshooting.md)

## Memory

Otto has a local, per-workspace/per-user memory store (`crates/otto`'s
`memory` module). It is enabled by default.

### Memory files

Records, tombstones and candidates are plain files under `~/.otto/memory/`
(mode 0700, files mode 0600). The location is not configurable and does not
follow storage routes. The files are the source of truth; there is no database
and no full-text index.

```text
identity.json          database and user scope ids
records/<id>.md        one active record
tombstones/<id>.json   one forgotten record, without its text
candidates/<id>.json   one pending or decided candidate
.lock                  writer lock
```

A record file has a frontmatter block of `name: <JSON value>` lines followed
by the record text:

```markdown
---
scope: {"namespace":"workspace","id":"otto-60ef11ccec92"}
kind: "preference"
key: "reply-language"
labels: []
confidence: 1.0
revision: 3
created_at: "2026-10-03T10:08:00.123456789Z"
updated_at: "2026-10-16T10:25:41.052684000Z"
expires_at: null
source: {"origin":"human"}
metadata: {}
---
Reply to technical questions in Chinese.
```

All eleven names are required once each; an unknown name is an error. The
record text is one line; a body with a second line is invalid. A workspace
scope id is the workspace directory name followed by the first 12 hex digits
of the SHA-256 of its canonical path, such as `otto-60ef11ccec92`; it does not
contain the rest of the path. A workspace initialized before this format keeps
its `sha256:<64 hex>` id; see [workspace identity](automation.md#workspace-identity).

Reads take no lock. Hand edits to a record file take effect at the next read,
normally the next turn's recall; Otto does not require a change to `revision`
or `updated_at`. Deleting a record file removes the record without a
tombstone. A file that fails parsing, validation or the secret guard is
skipped; memory open writes one `warning: memory file skipped: <path>: <reason>`
line to stderr per skipped file, and `otto memory status` lists each skipped
file with its reason. When two active records share scope, kind and a
non-empty key, the one with the later `updated_at` is used and the other is
skipped and reported. A record whose id has a tombstone is treated as
forgotten. Write new records with `/remember`; a hand-written file must be
complete and valid. The `bash` sandbox cannot write `~/.otto`, and the file
tools reject paths outside the workspace, so the model cannot edit these files
unless the sandbox is `off`.

Every write holds an exclusive lock on `.lock`, waiting up to
`[memory.turso].busy_timeout` (default 5 s) before returning a busy error. The
write re-checks the revision and key constraints under the lock, then writes
each file through a temporary file in the same directory and renames it into
place. A revision check compares revision numbers only, so a model update
that was read before a hand edit that did not change `revision` overwrites
that edit.

Limits:

- Writes that touch two files are not atomic. A crash between creating the
  record and marking its candidate decided leaves the candidate pending;
  accepting it again writes a second record with the same text, and recall
  de-duplicates identical text. A crash between writing a tombstone and
  removing the record file leaves the file; reads treat it as forgotten and
  opening the store removes it. A crash during a reflection proposal batch can
  leave part of the batch.
- Each recall reads every file in `records/`. Record text is at most 8192
  bytes. There is no index.
- Files are not encrypted at rest.

Config (`[memory]` in TOML; all keys optional):

```toml
[memory]
enabled = true
backend = "files"
required = false
recall_tokens = 2000
max_results = 12
require_encryption = false

[memory.turso]
path = "~/.otto/memory/memory.db"
busy_timeout = "5s"

[memory.workspace_ids]
"/canonical/path/to/workspace" = "stable-id"
```

`backend` defaults to `files`; `turso` and `sqlite` are accepted with the same
meaning, `remote` selects the [remote memory service](#remote-memory-service),
and other values are rejected. `[memory.turso].path` is only the
source of the one-time import described under
[Importing an earlier database](#importing-an-earlier-database); a leading `~/`
is the home directory. `[memory.turso].busy_timeout` is the writer lock wait.
`require_encryption = true` fails startup because the files are not encrypted.
`[storage.routes].memory` is a configuration error that names the key. There
are no `--memory-*` CLI flags.

### Rust memory service extensions

A Rust host can inject an independent `otto_core::memory::MemoryService` using
`otto::embed::MemoryExtension` and `OttoBuilder::memory_service`, or the custom
CLI entry point `cli::run::run_with_extensions`. Implementations declare their
capabilities; they do not implement Otto's `MemoryStore` file layout.
The shipped CLI reaches a service on another host through the
[remote backend](#remote-memory-service); it does not load Rust extensions.

```rust
// `service` is an Arc<dyn otto_core::memory::MemoryService> supplied by the host.
let extension = otto::embed::MemoryExtension::new(
    service,
    otto_core::memory::Scope {
        namespace: "user".into(),
        id: "host-user".into(),
    },
);
let otto = otto::embed::Otto::builder()
    .workspace("/path/to/workspace")
    .memory_service(extension)
    .build()
    .await?;
```

`new` borrows the provider; `owned` gives one host responsibility for shutdown.
Closing a session does not delete memories or close the shared provider.
Memory disablement, the content boundary and `require_encryption` still apply;
an unknown encryption guarantee does not satisfy that requirement.

The default extension mode is ReviewedChanges. It exposes read tools; a trusted
host obtains `Session::memory_service()` and `memory_context(origin)` to inspect
items and manage prepared changes. Existing native-record management commands
are unsupported for an independent service. They do not fabricate revisions or
write to a different local store.

`with_learning(LearningGrant)` explicitly authorizes delegated learning for
specified scopes, input classes and background processing. Supported model
`remember` operations submit explicit content; supported `forget` operations
request retrieval suppression. These tools return receipts distinguishing
completion, partial results, pending work and unknown outcomes. Input authority
is not approval of every derived memory. Returned content remains untrusted
reference material and cannot become a standing instruction through metadata.
`opaque_revision` uses the service's revision when supplied; a native numeric
revision is not substituted for it.

The only bundled network transport is the
[remote memory service](#remote-memory-service) protocol. See the
[service contract](../specs/2026-10-07-open-memory-contract.md) for capabilities,
prepared snapshot recovery and deletion coverage.

### Remote memory service

`backend = "remote"` selects an external memory service instead of
`~/.otto/memory/`. The service owns the memory; Otto keeps no local copy,
imports no data when switching backends and makes no startup network call.
This repository implements the client, **not** the service or its management
interface. Support ships with the normal
[Otto installation](../../README.md#install-from-source); no extra plugin or
daemon is needed. Local files remain the default backend. Once remote is selected,
recall and best-effort turn uploads need no further opt-in. Use the configured
runtime model; no special model selection is needed.

The [remote memory protocol](remote-memory-protocol.md) owns the exact API,
resolution and rendering contract. This section covers configuration and use.

#### Configure and start

Provision the external service, memberships and ACLs first. Put the following in
host user configuration (normally `~/.config/otto/config.toml`), not an untrusted
project overlay:

```toml
[memory]
backend = "remote"
recall_tokens = 8192     # remote default; rendered UTF-8 byte cap, not tokens
max_results = 12        # default

[memory.remote]
url = "https://memory.example.internal"
organization = "acme"
user = "alice"
token_env = "MEMORY_SERVICE_TOKEN"  # custom example name, not a default claim
recall_timeout = "3s"               # default

[memory.workspace_ids]
"/canonical/path/to/project" = "project-a"
```

Set the chosen variable privately in the environment that starts Otto; never put
its value in TOML, prompts, source files or shell history. The example explicitly
chooses `MEMORY_SERVICE_TOKEN`, not the built-in token variable name. For
`otto serve`/ACP, set it in that process's environment, not merely in the attaching
terminal. Restart affected runtimes after configuration or token changes.
Startup is not a service readiness check.

| Key | Default | Rule |
| --- | --- | --- |
| `url` | none, required | HTTPS; HTTP only for loopback (`localhost`, `127.0.0.0/8`, `::1`). No credentials, non-root path, query or fragment. A trailing `/` is removed; no redirects. |
| `organization` | none, required | 1..128 printable ASCII bytes, including ordinary spaces, with at least one non-space character; tenant boundary, never inferred from paths or model input. |
| `user` | none, required | 1..128 printable non-space ASCII bytes; selected subject within the organization. |
| `token_env` | explicitly chosen above | Environment-variable name: letters, digits and `_`, not starting with a digit, at most 128 bytes. |
| `recall_timeout` | `3s` | Go duration, greater than zero and at most `10s`; recall/get only. Writes and turn uploads use 30 s. |

Organization, selected user, token and host grants are host-authoritative:
project overlays or model arguments cannot select another subject or enlarge
permission. The service independently checks principal/subject delegation,
organization membership and project/write ACLs. Supplying IDs alone grants
nothing; a rejected selected scope fails the whole request.

- Only the named environment variable supplies the token. Its name/value are
  redacted from model-visible text and not passed to the bash sandbox.
- An unset/empty token disables memory for the process with one warning, unless
  `[memory].required = true`, which fails startup. Structural configuration
  errors always fail startup. No startup probe checks credentials or reachability.
- `require_encryption = true` fails for remote: Otto cannot verify service-side
  encryption at rest. Injecting a `MemoryExtension` while selecting remote also
  fails startup. `[memory.remote]` is otherwise read only for remote.
- `recall_tokens` is a rendered-byte bound, at most 8192 (the remote default),
  including the exact preamble, escaped attributes/text and newlines. Old
  markup-size estimates do not apply. `max_results` includes mandatory winners.
- Project scope uses the stable workspace memory identity/override, with a
  `<directory name>-<12 hex of the canonical-path SHA-256>` fallback; the
  service receives the directory name, not the path. Use a shared
  `[memory.workspace_ids]` override across hosts; organization remains part of
  identity. Without a project the
  context explicitly selects `workspace_id: null`.

#### Scopes, search and writes

Each request selects exactly one organization, one user and at most one workspace.
Organization memory is global to its organization; project memory requires project
membership. Personal memory is private to the selected user **within that
organization**, shared across their projects there, never implicitly across
organizations. Provisioning, membership management and service storage remain external.

For keyed conflicts, **workspace > organization > user**; without a project,
**organization > user**. Explicit keys identify semantic slots; free-text/null-key
contradictions cannot be guaranteed to resolve algorithmically. Pinning requires
inclusion, not higher authority: a lower pinned value makes its effective winner
mandatory. If mandatory winners cannot fit, all recall is discarded. Memory never
overrides host security, system rules or actual user authorization.

- `memory_search` accepts `query` **or** exact `id`, not both, and supports
  nullable `scope_filter`, a nonempty unique array of `organization`, `workspace`,
  `user`. It filters winning namespaces **after**
  resolution: a user-only view cannot expose a shadowed user value. Workspace
  filters require a selected project. Automatic recall always uses no filter.
- Exact-item `memory_search` (with `id`) is explicitly labeled **raw**. It may
  inspect a shadowed item, but is not effective lookup or automatically injected
  context. Remote references carry organization, scope, ID and opaque revision.
  Exact remote search additionally requires `organization_id`, `scope_namespace`,
  `scope_id` and `opaque_revision` from the reference. The organization must
  match the host-selected tenant and cannot change request context. Missing/
  mismatched tenants are rejected before dispatch, including old tenantless
  remote references. Wire get sends the exact scope/ID; it returns the current
  stored revision, not a historical revision lookup.
- `remember` takes `text`, nullable `key` and destination `scope` (`user` or
  `workspace` for model writes). Create uses the host-selected tenant and never
  silently upserts an active duplicate key. Remote update additionally requires
  `target_id`, `opaque_revision` and `organization_id`; `scope_id` validates
  a supplied reference's scope. A create may include `base_revision = 0`;
  a nonzero value requires `opaque_revision`. Updated key and text are full replacements.
  Generic metadata cannot set identity, authority or pinning; pin management
  is external service-admin functionality.
- Remote `forget` takes `id`, `organization_id`, `opaque_revision` and
  `scope_namespace`/`scope_id` (or `scope`). Legacy numeric `revision`/
  `base_revision` inputs remain for neutral/local compatibility, **not** as
  substitutes for remote `opaque_revision`. Organization inputs bind references,
  never select another tenant. On the wire opaque revisions use the field
  `revision`; they are never numerically compared.
- `forget` suppresses one user/workspace item, not its key across scopes or a
  physical-erasure promise. Suppressing a workspace override intentionally
  restores any still-active organization/user baseline.
- Ordinary model tools cannot organization-write even with admin credentials.
  Writes default to workspace when selected, otherwise user, never organization.
  Explicit human `/remember --scope organization` may request organization
  remember, still subject to service ACLs; human origin is not a privilege upgrade.
  Organization forget is out of scope; use the external service administration.

Restart with the configured environment, use an ordinary prompt to smoke-test
recall, then explicitly remember a harmless preference and ask Otto to search it:

```text
/remember --scope user --key response.language Prefer Chinese answers.
```

Inspect the receipt, not just acceptance: only applied/no-change establishes a
completed write; pending/unknown is not persisted. There is no Otto completion
polling endpoint; use the service's review/status interface.

#### Requests, uploads and failures

Before each textual user input Otto recalls with redacted input. It accepts only
complete, fully valid responses and injects escaped untrusted reference data.
Errors, partial/unknown coverage, invalid scope/order/fields/header or rendered
budget violations discard the **whole** result. The task continues without
memory, never an incomplete subset, another tenant or local-files fallback.
Organization memory is reference data, not outage-proof policy enforcement:
required organization rules need a separate authenticated host-policy channel.
Image-only inputs get no recall. Suppressed memory can remain in the current
turn's context until the next user input.

Explicit `remember`, `forget` and human `/remember` return receipts. Pending
means accepted for later decision, not completion. A write timeout, cancellation
or malformed response after possible delivery reports unknown outcome: the
service may already have applied it. There is no automatic retry or durable
local write outbox. Re-read exact state or use service status before a new write.
An explicit replay must reuse identical request ID/body within retention:
while it could commit and at least 24 hours after terminal resolution. Deferred
commits recheck ACLs, revisions and key uniqueness; revoked membership cannot
be bypassed by replaying a pending request.

Finished parent turns upload in the background: user text, assistant replies,
tool calls/results; no images/reasoning or separate subagent sessions. Subagent
final reports may appear in the parent's tool results. The service may extract
only authorized project or selected personal memory, never organization memory.
Acceptance is not extraction completion. Drop whole turns over 4 MiB or on
redaction markers/credential shapes (URI userinfo, private key delimiters,
credential headers/assignments). One worker, queue 8, nonblocking enqueue,
per-session order, no retry/spool/replay; failed/full-queue turns are lost.
Shutdown drains for at most 30 s; restart does not recover discarded work.

Local review/advice and `otto memory` subcommands remain unsupported for remote,
as do `/memory list`, `/memory show`, `/memory search`, `/memory forget` and
`/memory review`; they point to service management and never read the local files
store. `/remember` is supported with origin `human`: default destination as above,
`--scope user` for personal memory, or explicitly human `--scope organization`.
`--key` supplies authoritative key, not metadata; `--kind` remains non-authoritative
metadata. Automatic reflection does not run; `/reflect` and `auto = "on_exit"`
report memory unavailable.

The session shows fixed warnings/tool errors, never the service's untrusted
message. Each failed request logs endpoint/status/code and at most 512 bytes
of diagnostic message; logs omit token, queries, memory text and uploaded bodies.
See [Remote memory log lines](#remote-memory-log-lines).

| Failure | Actionable recovery |
| --- | --- |
| Missing token | Set the chosen variable in the runtime's environment and restart. |
| Unauthorized (401) | Check token validity and service principal/subject mapping. |
| Forbidden (403) | Check organization membership, delegation and project/write ACLs. |
| Conflict (409) | Re-read exact item/revision; resolve duplicate key or request identity, never blindly overwrite. |
| Invalid input/version (400) | Correct configuration/input or coordinate client/service schema deployment. |
| Too large (413) | Reduce explicit input; oversized turns are dropped. |
| Rate limited/unavailable/timeout | Check service/connectivity; continue without recall, explicitly try later if appropriate. Possible delivered writes remain unknown. |

#### Coordinated v1 cutover

This is an incompatible **in-place v1 replacement**, not a v2 or dual protocol.
Stop old clients, deploy the external service with the new schema/ACLs, update
Otto through its normal installation path, configure required
`memory.remote.organization`, restart and smoke-test recall/remember/search.
The unchanged `/v1/` path and `1.0` header do not imply old-body compatibility.
There is no default organization, automatic import/migration or old-wire fallback.
Old tenantless references fail safely; obtain new references in the selected
tenant. Session-history loading and public embedding constructors remain
compatible; remote embeddings must explicitly supply tenant coordinates.
The repository does not implement or migrate remote service storage.
Configuration changes take effect on runtime restart; local and remote backends
never synchronize each other's memory. Startup, turn acceptance and pending
receipts are not successful readiness/persistence checks.

#### Remote memory log lines

`otto serve`, `otto acp` and workflow commands write these lines to stderr as
`time=... level=WARN msg=<event> key="value" ...`. The embedding API writes
them to its diagnostics writer, or to stderr when none is set. The TUI, the
REPL and one-shot prompts do not write them.

Every frontend also writes the three `memory_turn_*` events as records in the
content-free [diagnostic files](servers-and-connectors.md#logs-metrics-and-request-correlation)
`~/.otto/logs/diagnostic-*.jsonl`: `event` is the `msg` value, the fields
are the same, and `time`, `pid`, `instance` and `version` are added.
`memory_request_failed` is not written there, because `message` is text from
the service.

| `msg` | Written when | Fields |
| --- | --- | --- |
| `memory_request_failed` | a request got a non-2xx status, an invalid response, a timeout or a transport error | `endpoint`, `status` (`0` without a response), `code`, `message` |
| `memory_turn_dropped` | a finished turn was not queued | `reason` (`over size limit`, the guard finding, `queue full`, `uploader closed`, `encode failed`, `id generation failed`), `turn_id`, `bytes` |
| `memory_turn_upload_failed` | the `/v1/turns` request for a queued turn failed | `turn_id`, `kind` |
| `memory_turn_drain_timeout` | shutdown stopped waiting for the queue after 30 s | none |

A failed upload writes both `memory_request_failed` and
`memory_turn_upload_failed`.

Limitations:

- One memory service per configuration.
- Turns that are not delivered are lost; about 2.5% of turns measured on
  2026-10-17 contained a redaction marker and were not uploaded.
- Workspace scopes match across hosts only through `[memory.workspace_ids]`.
- An Otto release without this feature rejects `backend = "remote"`.
- Managed policy settings such as `allowed_provider_hosts` do not restrict
  the memory service `url`.
- In the TUI, the REPL and one-shot prompts, a failed or skipped turn upload
  shows no session warning; it is recorded only in the diagnostic files. The
  HTTP status of a failed upload is in `memory_request_failed`, which these
  frontends do not write.

### Importing an earlier database

Earlier releases kept memory in a Turso database: `[memory.turso].path`
(default `~/.otto/memory/memory.db`), or the endpoint of a published local
`memory` storage route. When `~/.otto/memory/identity.json` is absent and that
database exists, the first start of this version imports it once:

- Stop every Otto process before the first start. Writes that an older process
  makes to the database after the import are not in the files.
- The database must have memory schema version 2. A legacy SQLite `memory.db`
  from before 2026-10-03 must first be migrated with `otto storage migrate`
  from Otto 0.4.8.
- A published `memory` route to an external backend fails the open. Transfer
  memory back to local storage with a 0.4.8 build first.
- Active records, tombstones, candidates and the identity ids are copied with
  their ids, revisions and timestamps. Pending approvals stay attached to their
  candidates. Every row is validated before any file is written; a failing row
  fails the import with its id and writes no file. `[memory].required`
  decides whether that stops startup or disables memory.
- `identity.json` is written last, so an interrupted import repeats at the
  next start.
- The database file is never modified. Otto does not read it again after the
  import. After checking the files, delete it manually.

With no database and no `identity.json`, Otto creates `identity.json` with new
ids.

### Migrating other legacy databases

Stop every Otto process using the database, then run:

```bash
otto storage migrate ~/.otto/usage.db ~/.otto/usage-turso.db
```

The destination must be a new path in an existing directory. The command
requires Python 3 for a read-only export of the legacy SQLite file, imports
into a temporary Turso database, checks row counts, and preserves the source.
It supports `usage.db`, `tasks.db`, `reflection.db`, `skill-checks.db`,
`memory-advice.db`, and workspace workflow databases; their recognized legacy
tables import into local Turso databases. Memory is not a storage domain and
`otto storage migrate` does not accept a memory database. For stores with
fixed paths, keep the old file as a backup and move the migrated file into
place while Otto is stopped. Do not let an older SQLite-backed Otto and this
version access a database at the same time.

To transfer a stopped local Otto installation to a separate local root, use
`storage transfer`:

```bash
otto storage transfer "$HOME" /Volumes/otto-state-new migration-20261006
```

Use an existing destination parent on a local volume outside the source home;
the destination root itself may be new. The source and destination roots must
not overlap. Repeating the same source, destination, and migration ID resumes
an interrupted transfer. The
transfer covers the storage domains only; `~/.otto/memory/` stays in the
source home and is not copied, and pending memory reviews stay valid. A
route authority that an Otto 0.4.8 or earlier transfer published with a
`memory` route is refused with `published storage routes include memory`.
On success, the source home's
route authority points to the new root; restart Otto using the same home to use
the transferred state. Original databases and session files stay in place.
Keep them until the restarted Otto has opened the destination successfully;
cleanup is manual. The transfer requires local paths and does not offer a
remote/cloud destination.

If transfer fails before route publication, explicitly restore source admission:

```bash
otto storage abort "$HOME" migration-20261006
```

Abort retains the source, staged bytes and checkpoints. It also works after
partial staging imports; it refuses a published destination or an uncertain
publication that cannot be reconciled. After publication, returning to the old
root requires another verified transfer so new destination writes are preserved.
Stop all source processes, including older Otto versions, and prevent them from
restarting throughout transfer; local locks cannot fence an uncooperating binary.

The ordinary binary keeps its local Turso and confined POSIX defaults. It does
not include MySQL, PostgreSQL or cloud backend implementations. Extension
binaries can link and register additional domain repository factories; their
backend names and supported domains are supplied by those extensions.

For a linked extension, user or managed configuration can name backend
instances and route domains to them. Project configuration cannot redirect
storage, even in a trusted workspace. A service endpoint is an environment
binding rather than an inline DSN; credentials belong only in that environment
value. Driver-specific `options` must be non-secret and supported by the factory.

```toml
# Example for a binary that actually registers a backend called "postgres".
# The ordinary Otto binary rejects this unregistered backend.
[storage.backends.primary]
backend = "postgres"
endpoint_env = "OTTO_POSTGRES_DSN"

[storage.routes]
approvals = "primary"
workflows = "primary"
schedule = "primary"
usage = "primary"
tasks = "primary"
reflection = "primary"
skill_checks = "primary"
memory_advice = "primary"
```

Normal startup opens existing published namespaces. Changing this configuration
does not create an empty replacement database or activate a new endpoint. A
fresh installation can explicitly initialize an extension's namespace with
`otto storage init <home> <config-path>`; initialization refuses existing local
authority state or an existing published route group. Namespace identities are
checked again on restart. Keep the same captured environment bindings available.

For existing data, a linked extension that implements logical import/export can
select configured destination domains:

```bash
otto storage transfer "$HOME" /Volumes/otto-state-new migration-20261006 \
  --to-config /absolute/path/to/destination.toml
```

The filesystem destination still retains isolated local state, session journals
and transfer ownership metadata. Domains selected by the destination config
use the extension's isolated namespace; remaining domains use their normal
local destination. Every selected domain must exist in the source snapshot.
The transfer validates all linked state, copies bounded chunks, verifies actual
staged records and receipts, then publishes one route generation. Install the
matching user/managed configuration before restarting; its preferences must
agree with the publication. Repeating the same command resumes or reconciles
the original receipt and refuses a changed namespace. Source and staging data
remain until explicit cleanup. The existing `storage abort` and stopped-process
rules apply. A reverse transfer can return state to local Turso/POSIX storage;
remove the old route preferences only after that transfer publishes.

Extensions need the declared domain profiles, migrations and conformance gates.
Sessions, workspace files and executable trust retain their existing local
boundaries. This is an offline procedure for coordinated processes on one host;
local admission locks do not establish distributed writer fencing.

To inspect usage windows without opening the file with SQLite:

```bash
otto storage window-peaks ~/.otto/usage.db <session-id>
```

This prints JSON containing each context window's maximum input-token count
and request count. It opens the database read-only.

What's wired:

- Recall before each turn into a request-local context block that is never
  written to Pi session JSONL, compaction summaries, or logs, so compaction
  does not remove it. The block has two parts:
  - Standing preferences: every active user-scope `preference` or
    `instruction` record that has a key, oldest first, under a header that
    tells the model you approved each one and to follow them unless the
    current request says otherwise. They are recalled on every turn whatever
    the message says, and do not count against `max_results` or
    `recall_tokens`. See [Standing preferences](memory-and-extensions.md#standing-preferences).
  - All other active, unexpired records in the session's user and workspace
    scopes, under a header that marks them untrusted reference material. A
    workspace record replaces a user record with the same kind and key, and
    identical text is listed once. Workspace records come first, then newest
    `updated_at` first, until `max_results` or `recall_tokens` is reached. The
    message text is not matched, so a workspace with records adds up to
    `recall_tokens` input tokens (default 2000) to every turn; lower
    `recall_tokens` to reduce the cost.
- Agent tools: `memory_search`, `remember`, `forget`.
- Human commands in both frontends: `/memory list`, `/memory show`, `/memory search`, `/memory forget`,
  `/memory review`, `/memory review <id> accept|reject`, and `/remember`.
- `/reflect [focus]`, `/reflect list`, and `/reflect show <id>` in the TUI, the REPL, and
  the Web UI: reflection, described below.
- Agent tool: `reflect`, which runs the same reflection when you ask for a retrospective.
- Standalone CLI: `otto memory status`, `otto memory list`, `otto memory show <id>`, and
  `otto memory forget <id>`.

`memory_search` and `/memory search` accept `query`: it is split on whitespace
(at most 64 terms), and a record matches when any term is a case-insensitive
substring of its text, kind, key or one of its labels. Matches list workspace
records first, then by the number of matching terms, then newest `updated_at`
first. An empty query lists recent records. `memory_search` also accepts
`id` for an exact record lookup within the current session's user and workspace
scopes; `id` and a non-empty `query` are mutually exclusive, and an empty `id`
is treated as omitted. Before proposing a deletion by ID, the agent can use
`memory_search({"id":"<record-id>"})` to read the current scope and revision,
then call `forget`. Search results stored in session history retain only IDs;
the full record is available to the model during the search turn. In Telegram
and Feishu, a deletion needing human review uses the memory
card or `/memory accept <candidate-id>`.

`/memory list` and `otto memory list` page active records in stable update-time order. They
show the current user and workspace scopes by default; use `--scope user` or `--scope workspace`
to narrow that set, or explicit `--scope all` to inventory every stored workspace. A page prints
`next_cursor=...` when another page exists; pass that value back with `--cursor`. `show` reads one
record from the current scopes (or the selected user/workspace scope).

Model-originated writes first land as candidates. The shared approval policy
reviews them; AUTO can apply changes supported by the user's requested content
and scope, while requests needing a user decision remain pending.
`/memory review` lists the pending candidates for the current user and workspace scopes (id,
scope, action, kind, key, text, reason, confidence, origin), and `/memory review <id> accept`
or `/memory review <id> reject` decides one. Accepting creates the active record (or applies the
update or forget); rejecting leaves memory unchanged.

#### Advice on candidates

Before a candidate from `remember` or from `/reflect` is queued, Otto asks a
model three questions: whether it applies to all workspaces or only this one,
whether it duplicates or conflicts with a standing user-scope record, and
whether it is durable or a one-off request. The answers are stored with the
candidate and shown as one line in `/memory review`, in the TUI review
dialog, and as the `advice` field of `_otto/memory/pending` (so the chat
connector shows it too), for example:

```text
Advice (session model gpt-x): all workspaces · duplicates plan-first · one-off request
Advice (auxiliary model jev-latest): all workspaces 91% · new 84% · durable 77%
```

When the advised scope differs from the candidate's, the scope part ends with
` (proposed: workspace)` or ` (proposed: user)`. Advice is a hint only: it
never accepts, rejects or changes a candidate. By default the question goes to the
session's own provider, model and thinking setting (token usage is recorded under
the task id `memory-advice`); when `TYPESAFE_API_KEY` is set and non-empty it
goes to the `jev-latest` model at `https://api.typesafe.ai`, which also gives
percentages. The auxiliary model is used for this advice, automatic-reflection
scheduling hints, and skill checks: it does not run turns or tools, and it is not a provider or
a model you can configure. The request holds the candidate's kind, key, scope, text and
reason, and the keys and texts of the user-scope standing records. A candidate
that the memory service refuses is never sent. Each request has a 20 s limit; on
a timeout, an error or an unusable answer the candidate is queued without
advice. Each exchange is appended to `~/.otto/memory-advice.db`, table
`memory_advice`, with the columns `created_at`, `model`, `auxiliary`,
`candidate_id`, `scope`, `request` (the body or prompt as sent, never the API
key), `response`, `outcome` (`ok`, `timeout`, `error` or `invalid`), `error`
and `latency_ms`. A failed write there does not stop the candidate from being
queued and is not reported.
Human `/remember` and `/memory forget` apply immediately.

When a session opens (start, `/new`, or resume) and the current user and
workspace scopes hold pending candidates, Otto shows one line, for example
"2 memory proposals are pending review (oldest since 2026-10-01). Run /memory
review to accept or reject them." The REPL and the TUI print it before the
first prompt, and a local `otto acp` session sends it as a notice for that
session. `GET /v1/sessions/{id}/notices` returns it, but the Web UI and ACP
connections through `otto serve` (`otto acp --attach` and ACP TCP) start at the
current notice cursor and do not show it; the chat connector shows pending
candidates after each turn instead. `otto memory status` lists each memory file that was skipped as invalid, with
its reason, and prints the number of storage-level pending candidates in the same scopes; this diagnostic count can
include a candidate whose approval is no longer awaiting human review. A
candidate stays pending until you decide it; it does not expire, and its
one-shot review remains visible after the session that proposed it ends.

ACP memory review pages use the memory store's opaque snapshot cursor. If a
candidate changes between page requests, `_otto/memory/pending` returns a memory
conflict; restart with an empty cursor to read the new actionable set without
silently skipping a candidate.

### Standing preferences

A working habit that should apply in every workspace, such as "explain the plan
before executing a task", belongs in the standing set: user scope, kind
`preference` or `instruction`, and a key. Standing records are recalled on every
turn, so they apply even when the message never mentions them. Ways to add one:

- `/remember --scope user --kind preference --key plan-first Explain the plan before executing a task.`
  writes it immediately.
- The model's `remember` tool takes `scope: "user"` for a habit that applies in
  every workspace (the default is the workspace), and reflection proposes
  `scope: user` for the same kind of habit. Both queue a pending candidate that
  applies after the shared approval service authorizes it.

A user-scope `preference` or `instruction` without a key is rejected, because
only keyed records are standing.

The standing set holds at most 2,000 estimated tokens. A `/remember`, or an
accept in `/memory review`, that would push it past that fails with an error
that states the tokens used and needed; a rejected accept leaves the candidate
pending. Forget an existing standing record with `/memory forget <id>`, then
accept again.

`otto memory status` prints the standing usage, for example `standing: 5
records, 840/2000 tokens`. Records written before this limit existed can exceed
it. Recall then injects records oldest first and stops at the first one that
does not fit, the status line appends
`, N not injected (over budget)`, and each session open shows one line with the
number of standing preferences that are not recalled. `/context` shows the
memory text sent with the previous turn, including the standing records.

### Reflection

`/reflect [focus]` looks back at the part of the current session that no earlier
reflection covered and asks the session's own model, once and with no tools, for a short
retrospective of it (what was done, what took detours or failed and why, and what to do
differently), which durable facts and preferences are worth remembering, and which
procedures are worth reusing as skills. `/reflect` prints the result line followed by the
retrospective, and the retrospective is the first section of the run log described below.
Each memory survivor is queued as a pending candidate (origin
`extractor`) for the shared approval service; candidates requiring a human decision
remain available in `/memory review`. Reflection never writes a memory
record itself, and it appends nothing to the session file. Skills are different: a skill
that passes every check below is published as an approval request, and the file is
written only after that request is approved, as described under
[Generated skills](memory-and-extensions.md#generated-skills). `focus` is free text that steers
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
transcript text). Each run that starts, whether it completes, proposes nothing, fails, or is
canceled, also writes a Markdown log to `~/.otto/reflection/runs/<time>-<id>.md` (mode 0600;
the directory is mode 0700). The log lists every proposal the model returned, in two groups
(memories, then skills), with its text, the reason the model gave, the entries it cited, and
its outcome: `queued as candidate <id>`, `approval request <id>`, or `dropped: <reason>`.
It also lists the ids of the external entries whose content was withheld, why skills were not
requested when they were not, and, for a run that failed on an unparseable answer, the raw
answer. The retrospective comes first, quoted so its Markdown cannot pose as another
section, and is cut at 8 KiB. Each text field is redacted like a request, and a field that
still matches a secret rule is replaced by `(withheld: <rule>)`; a field is cut at 16 KiB. A run that is skipped
(the cooldown or `min_turns`) or that fails before it starts writes no log. Otto does not
remove log files.

When `TYPESAFE_API_KEY` is non-empty in Otto's environment, automatic reflection
has an additional scheduling hint, enabled without a new config setting. After
the normal trigger, cooldown, dynamic-content boundary and minimum-turn checks,
the auxiliary backend receives the bounded, redacted session evidence (not
standing memories or skill contents) and chooses `immediate` or `defer`.
`defer` writes no completed run and does not advance the watermark, so later
automatic triggers or `/reflect` can reconsider the same evidence. There is no
background retry timer. Manual `/reflect` and model-requested `reflect` always
bypass the hint. A timeout (20 s), unavailable backend or invalid answer runs
the original reflection pipeline; cancellation still cancels the run. Confidence
must be finite and in 0–1, and is observational only, never permission to accept
memory, write skills, grant access or change standing instructions. Existing
memory review and skill approval boundaries remain unchanged.

The content-free `reflection_decision` event records fixed backend/trigger/outcome
and reason codes, latency, input byte count and optional confidence, in the process
log and enabled diagnostic file sink. Gate exchanges, transcript text and backend
error text are not persisted. If an automatic run is deferred but a review is
needed now, use `/reflect`; if the auxiliary service is unavailable, no action is
required for reflection to continue. Removing the environment key and restarting
Otto restores the ungated automatic path (and the existing session-model advice
fallback). No database migration or version-specific config change is required;
retained session evidence remains available across restart.

A completed run advances a per-session watermark, so the next run covers
only newer entries; a failed or canceled run leaves it in place and the next run covers the
same entries again. Provider usage is recorded in the usage history under the task id
`reflection:<run id>`. A session started with `--no-session` has no file to read and cannot
be reflected on.

#### Asking the agent for a retrospective

When you ask the agent in plain words for a retrospective, a review of the session, or to
distil lessons or skills from it, it calls the [`reflect`](safety-and-tools.md#reflect) tool, which runs the
same reflection as `/reflect` and returns its result line, run log path and retrospective.
The agent then summarizes them; it does not write a retrospective file of its own. The
result line also appears as a reflection notice in every frontend. The run is recorded with
trigger `requested`, ignores `[reflection].auto` and the automatic cooldown, and runs beside
the turn that asked for it, so it covers the session up to and including your request.

#### Past runs

- `/reflect list` shows the 50 newest run logs, newest first: the id, the start time (UTC),
  the trigger, the status, and the retrospective's first line.
- `/reflect show <id>` prints one run log in full, with its path.

They work wherever `/reflect` does: the TUI, the REPL, the Web UI, ACP (including
`otto acp --attach`) and the chat connector. Only the exact words `list` and `show <id>` are
commands; `/reflect list what went wrong` is a run with that focus.

#### Generated skills

When a run's slice is clean, the model may also propose skills: short task-scoped
procedures that you asked for or approved and that ran successfully in the slice. A
proposal that passes all of these, in order, is published as an approval request that shows
the skill name, the target path, and the full content, including every Python script. `~/.otto/skills/<name>/SKILL.md` is
written only when that request is approved; before then nothing is written. On approval the
write is checked again: it fails if the cited session entries no longer pass the checks or
if the existing skill file changed since the request was made. When no approval service is available the skill is dropped
(`skill_approval_unavailable`). Any failure drops the skill and is counted; nothing is partly
written.

1. **Source isolation.** If the slice contains any external entry (see above), skills are not
   requested at all and the run says so. `skill_source = "any"` lifts this; external entries
   are then still withheld and cannot be cited.
2. **Evidence.** A skill must cite one of your messages and one entry showing the procedure
   ran (a successful tool result, or an assistant tool call), with verbatim quotes.
3. **Structure and rule scan.** A valid name and description, a body of 40 characters to
   16 KiB, at most four Python scripts of 16 KiB each under `scripts/`, and no match in a fixed rule table: secret patterns and any value Otto knows to
   be secret, instruction-override and conceal-from-the-user phrasing (English and
   Chinese), tampering with the sandbox, approvals, or `~/.otto` state, pipe-to-shell,
   destructive and data-egress commands, URLs that no cited entry contains, invisible
   Unicode, and large encoded blobs. This is a tripwire for known-bad shapes, not a proof of
   safety.
4. **Name and ownership.** A new skill never reuses a name that exists in any configured skill
   root. Reflection revises only a skill it wrote whose complete file set is unchanged
   since; a skill you wrote, or a generated one you have since edited, is never revised or removed by it.
5. **Python syntax.** When scripts are present, Python 3 must be available as `python3` on
   PATH. Otto checks their syntax with an isolated interpreter in a temporary directory
   without importing or executing the generated code or writing bytecode. A missing
   interpreter, invalid source, or a validation timeout drops the proposal before approval.
6. **Model review** (`skill_review`, default on). A second tool-less call sees only the
   candidate, including script source, never the transcript, and must answer `ALLOW`; an
   error, a timeout, or any other answer rejects the skill.

Reflection may turn a successfully performed, repeatable procedure into a skill containing
`SKILL.md` and `scripts/*.py`. The Markdown explains when to use it, inputs, outputs,
invocation and error handling; Python holds the deterministic work. Script names use
lowercase letters, digits, underscores and hyphens, with no nested directories. Every
revision supplies the complete file set, so a removed script is removed on publication.
The complete serialized proposal is capped at 48 KiB to fit the existing approval request.
For example, `/reflect Extract the successful report calculation into a reusable Python skill`
steers a manual run toward this output; generation still requires the existing evidence checks.

Approving the skill installs its files; it does not run them or grant execution permissions.
A later task loads the skill with the `skill` tool and invokes Python through the normal
`bash` tool using the skill location and current workspace. Existing sandbox and approval
rules apply. Restart Otto if the skill root was created after startup and the shell cannot
read it, so sandbox read paths are refreshed. Syntax validation does not prove that a
script produces correct results.

The entire skill is versioned and published together. Editing a script or adding a file
transfers ownership to you, just as editing `SKILL.md` does. `/skill revert <name>` restores
or removes the whole generated bundle, including scripts. Older Markdown-only skills and
history remain readable without migration.

Generated `SKILL.md` frontmatter carries only `name` and `description`: it never declares an `input`/`output`
contract, so it is never run as a sub-agent, and it has no `allowed-tools`. At most
`max_skills` are proposed per run and `max_generated_skills` may be owned in total. The
`/reflect` result line reports the number of pending skill approvals; after an approved
write, the skill becomes visible to the model
at the next catalog discovery (`/new`, `/resume`, `/model`, or a restart), like any new skill,
and can be undone with `/skill revert <name>`.
Skills are written only to `~/.otto/skills`; if you changed `[skills].paths` so that root is
not listed, they are written but not discovered.

- `/skill generated` lists the skills reflection wrote, with the run, the session, the time,
  and the reason it gave; one you have edited is marked as no longer owned.
- `/skill <name>` on a generated skill shows where it came from.
- `/skill revert <name>` restores the previous version, or removes the skill if reflection
  created it. It refuses a skill you have edited. Every version reflection wrote is kept under
  `~/.otto/skill-history/` (`.md` for Markdown-only versions, `.json` for bundles), outside
  the skill roots, so it is never loaded.
- `/skill set <name> disabled` also works on a generated skill.

The residual risk is stated plainly: a skill is instructions the model follows in later
sessions, and the checks above reduce but cannot remove the chance that a plausible-looking
bad procedure is written. Set `skills = false` to remove it. Under the default settings a
slice that held external content never produces skills.

```toml
[reflection]
enabled = true            # default true; false disables all reflection and does not open reflection.db
auto = "on_work"          # "on_work" (default) | "on_compaction" | "on_exit" | "off"
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

Reflection starts automatically as `[reflection].auto` selects. It is on by default
(`on_work`) and may propose memories or write vetted skills. Set `auto = "off"` to keep
only `/reflect`, `skills = false` to stop skill writing, or `enabled = false` to turn
reflection off entirely.

- `on_work` (default): after a completed agent turn with at least ten distinct successful
  tool operations, reflect in the background over the uncovered session entries. Failed
  tools do not count, and an errored, cancelled, or approval-pending turn does not qualify.
  Separately resumed approval continuations have their own counts. This mode also retains
  the successful-compaction trigger below.
- `on_compaction`: reflect only after a successful compaction, whether automatic or started
  with `/compact`. A session that never compacts never reflects automatically in this mode.
  An existing explicit `auto = "on_compaction"` setting retains that behavior.
- `on_exit`: when a terminal frontend (the TUI, the REPL, or `--prompt`) ends normally,
  reflection runs once over what no earlier run covered, if the session has at least
  `min_turns` user messages. **This delays exit** by as long as the run takes, up to 60
  seconds, while a line says so; Ctrl+C skips it. A killed process, a Ctrl+C exit, `otto serve`
  and `otto acp` do not run it. A session shorter than `min_turns` is skipped with no record, so
  `/reflect` can still cover it.
- `off`: only `/reflect`.

Background runs never delay the task response and are single-flight. `on_work` and
`on_compaction` share a ten-minute cooldown per session, counted from the previous run's
start; a failed run counts too. All existing evidence, source-isolation, and skill-review
checks still apply. `/reflect` is not held back by this cooldown.

Every automatic run that starts queues a start line (`reflection: running (<trigger>)`) and,
when it ends, one result line. The result line of a completed run names the run log, also
when the run proposed nothing; a failed run reports its error once, with the log directory. A
canceled run and a skipped run queue no result line. The REPL, TUI, and Web UI
show these lines. ACP sends new notices as tagged agent message chunks, and the chat
connector sends them after the active task reply. A connection through `otto serve`
starts at the current notice cursor without replaying older notices; a local
`otto acp` session also sends the lines queued when the session opened. Use `/memory` and `/skill generated` in
chat to inspect pending proposals and generated skills.

Not yet implemented:

- `Binding.Observe` is not wired; reflection reads the session file instead.
- The Web UI and the HTTP API cannot review memory candidates; use `/memory review` in a terminal.
- Reflection never changes a skill you wrote. Generated supporting files are limited to
  Python scripts; arbitrary assets are not generated.
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
typesafe_skill_check = true                 # default true
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
  [sub-agent execution design](../specs/2026-09-22-skill-subagent-execution.md)
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
  [reflection](memory-and-extensions.md#reflection) wrote.
- `/skill` lists every available skill; `/skill <name>` displays its description,
  location, contract-check status, and instructions; `/skill set <name>
  enabled|disabled` persists its state change. `/skills` is not a command.
- Discovery runs at startup and on `/new`, `/resume`, `/model`; the catalog is
  fixed within a session. A loaded body is a normal tool result stored in the
  session and re-sent on every later request until compaction.

Not yet implemented:

- `allowed-tools` enforcement.
- Reading `~/.claude/skills`; hot reload inside a session.

### Automatic contract check

When enabled, the first time a session delegates to a given version of a
skill's `SKILL.md` (a skill with an `input`/`output` contract, run through the
`agent` tool), Otto sends the full `SKILL.md` text, including its
frontmatter, to `https://api.typesafe.ai` for an automated review of whether
the skill's instructions are self-contained and its declared contract
matches what the body does. A skill flagged by a local pattern check (for
example, an `output` field too vague to be useful) is judged locally and
never sent. Results are stored in `~/.otto/skill-checks.db` (Turso,
append-only) and shown in `/skill <name>` for contract skills.

The check is enabled by default when skills are enabled. It runs when both of
these conditions hold:

```toml
[skills]
enabled = true                 # default
typesafe_skill_check = true    # default
```

and `TYPESAFE_API_KEY` is non-empty. Set `typesafe_skill_check = false` to
disable it. When disabled or when the key is missing, nothing is sent and
`~/.otto/skill-checks.db` is never opened. The old
`[experimental].typesafe_skill_check` setting is still accepted for
compatibility when the `[skills]` setting is absent.

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
  [command-line reference](getting-started.md#command-line-reference).
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

## Plugin packages

Plugins package reusable Skills and MCP servers. Otto supports the Skills and
MCP components of [Agent Plugins 1.0](https://agent-plugins.org/), plus a limited
adapter for `.claude-plugin/plugin.json` and `.codex-plugin/plugin.json`.
Support is enabled by default; no installed packages means no added capability.

A portable package has these root locations:

```text
my-plugin/
  plugin.json
  skills/review/SKILL.md
  mcp.json
  server.py
```

`plugin.json`:

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "my-plugin",
  "version": "1.0.0"
}
```

`mcp.json` (optional):

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "tools": {
      "type": "stdio",
      "command": "python3",
      "args": ["${PLUGIN_ROOT}/server.py"]
    }
  }
}
```

Skills use Markdown; MCP servers can use Python, TypeScript/JavaScript, Go,
Rust or any other language implementing MCP. Install the required interpreter
and dependencies yourself. The plugin installer does not run npm/pip installation scripts. Otto does not
load language modules into its own process. Remote servers use
`type: "streamable-http"` and `url`; HTTPS is required except on loopback.
Legacy HTTP+SSE is unsupported.

Manage packages without configuring an AI provider:

```bash
otto plugin install ./my-plugin
otto plugin install https://github.com/example/my-plugin.git --rev <full-commit-id>
otto plugin list
otto plugin inspect my-plugin
otto plugin trust my-plugin
otto plugin disable my-plugin
otto plugin enable my-plugin
otto plugin revoke my-plugin
otto plugin remove my-plugin
```

Git installation requires a full commit ID, not a branch or tag. Git hooks,
global/system Git filters and submodules are not run. Authentication can use
SSH agents or a configured `GIT_ASKPASS` helper. Global/system Git configuration
and `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` overrides are excluded from install
subprocesses to prevent package-triggered filters or hooks. URLs containing passwords,
HTTPS usernames, query parameters or fragments are rejected.

Installation copies a package into a versioned content snapshot under
`~/.otto/plugins/packages/`. It enables Skills, but MCP requires the explicit
`trust` command. Inspect the source and execution configuration before granting
trust. Trust is tied to the installed content, including executable modes;
changed content must be reinstalled and trusted again. The command authorizes
both MCP process startup and HTTP connection. Stdio follows Otto's existing
MCP execution model and runs outside the Bash sandbox.

Enable/disable/installation changes apply at the next runner build, normally a
new session or restart. If a package is installed while Otto is running,
restart or run `/sandbox reload` before Bash scripts can read its new snapshot
on macOS. Revoking or removing a package blocks subsequent MCP
calls in active sessions and closes its servers. Removal retains package
snapshots and `~/.otto/plugins/data/<name>/` so existing Skills and persistent
data remain available; automatic snapshot/data garbage collection is not
implemented. Package name replacement requires removal followed by installation.

Skills appear as `my-plugin:review`; their local `SKILL.md` name remains
`review`. MCP components have logical IDs such as `my-plugin:tools` and bounded
internal IDs shown by `inspect`; the tool search/router exposes their tools.
This keeps equal component names from different plugins separate.

User-owned configuration can disable packages and supply credentials:

```toml
[plugins]
enabled = true
# disabled = ["my-plugin"]

[plugins.bindings."my-plugin:tools".env]
API_TOKEN = "${MY_PLUGIN_API_TOKEN}"

# For an HTTP component instead:
# [plugins.bindings."my-plugin:remote".headers]
# Authorization = "Bearer ${MY_PLUGIN_API_TOKEN}"
# [plugins.bindings."my-plugin:remote"]
# auth = "oauth"
```

Never place literal API tokens in a package. Bindings use the existing MCP
credential expansion/redaction rules and may also set `auth`,
`oauth_client_id` and `oauth_scopes` for HTTP servers. OAuth cannot be combined
with an Authorization header. Use the logical server ID (`my-plugin:remote`) or internal ID from `inspect`
for `otto mcp login <server>` when OAuth is configured. `otto mcp list` includes
plugin servers with both IDs. Project configuration may
add disabled names or set `enabled = false`, but cannot grant trust, re-enable
user-disabled packages or set credential bindings. Managed MCP restrictions
also apply. `[skills].enabled = false` and `[mcp].enabled = false` disable the
corresponding plugin components.

Portable MCP expands only `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` in stdio
arguments, environment values and working directories. The subprocess also
receives both variables. Command, URL and header fields do not expand host
variables. A command is one bare executable name or `./package/path`, never a
shell command line. Working directories remain beneath the plugin root or its
persistent data root.

Legacy adapters support conventional/custom Skills paths and file/inline MCP
configurations; package-root placeholders receive limited translation. Legacy
Hooks, Commands, Agents, LSP, OpenAI Apps and Pi runtime code are not activated.
Otto-specific hooks/tools use `extensions.dev.otto`, as described below.
Recognized unsupported components and invalid independent components produce
diagnostics while valid Skills/MCP remain usable. This is not full Claude,
Codex or Pi plugin compatibility. Skill `allowed-tools` remains parsed without
permission enforcement.

Session history records active package names, versions and content digests as
append-only metadata. Resuming history does not install or authorize packages
listed in old metadata; the current installation registry determines capability.

## Hooks and custom tools

A runnable [hook-demo example](../../examples/plugins/hook-demo/README.md) includes
a Python hook, a custom tool and an offline protocol check.

Lifecycle hooks and external command tools are available in the Otto-specific
`extensions.dev.otto` namespace of an installed `plugin.json`. Existing Skills,
MCP formats, snapshot installation and content trust remain the package contract.
Commands require `otto plugin trust <name>` for the installed digest. Untrusted
packages still contribute Skills, but omit hooks/tools with a diagnostic.
Enable/disable affects new runners; revoke/remove blocks existing command
snapshots without replaying their effects.

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "demo",
  "version": "1.0.0",
  "extensions": {
    "dev.otto": {
      "version": 1,
      "hooks": {
        "before_tool": [{
          "matcher": ["bash", "mcp__github__*"],
          "command": ["python3", "${PLUGIN_ROOT}/check.py"],
          "timeout": "10s"
        }]
      },
      "tools": [{
        "name": "inspect",
        "description": "Inspect project metadata",
        "command": ["python3", "${PLUGIN_ROOT}/inspect.py"],
        "input_schema": {
          "type": "object",
          "properties": {"path": {"type": "string"}},
          "required": ["path"],
          "additionalProperties": false
        }
      }]
    }
  }
}
```

Commands are argv arrays, without implicit shell interpretation. Package
arguments expand `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` using the installed
snapshot and existing data directory. They always start in the selected
workspace, run through the current sandbox and use its filtered environment.
No additional filesystem grants or credential bindings are implied: write
state inside the workspace unless the selected sandbox already permits its
destination. Only explicit sandbox `off` is unconfined. A hook cannot request
elevation or grant authorization. On Linux, commands need explicit sandbox
`off`, as with Bash.

Standalone hooks can also be configured in user or trusted project TOML:

```toml
[[hooks.before_tool]]
matcher = ["bash"]
command = ["python3", ".otto/hooks/check.py"]
timeout = "10s"
```

Project declarations require `otto trust <workspace> --executables`. Hook
configuration, `.otto/hooks` and explicitly referenced project hook files are
bound to that grant. Standalone commands snapshot referenced workspace files
and reject changes until a new runner is built. Standalone hook arrays follow
ordinary configuration precedence; a higher layer replaces the array for that
event. Standalone hooks run first, then packages sorted by name; entries run
in declaration order.

All native frontends (CLI, TUI, HTTP/Web and ACP), sub-agents and manual/automatic
compaction use the shared hook path. Tool matchers use exact names or one `*`;
omission matches every known tool invocation. For `mcp_call_tool`, matching
uses the selected remote name returned by MCP search, and arguments remain the
router's `{tool, arguments}` object. Child tool allowlists and write-path checks
apply to rewritten arguments before dispatch.

### Hook protocol v1

One command receives a JSON object on stdin, followed by EOF:

```json
{
  "version": 1,
  "event": "before_tool",
  "workspace": "/path/to/project",
  "session": {"session_id": "s1", "parent_session_id": null, "task_id": "", "mode": "new"},
  "data": {"turn_id": "op_...", "tool": "bash", "tool_call_id": "call1", "operation_id": "op_...", "attempt": 1, "arguments": {"command": "git status"}}
}
```

Top-level fields are stable within v1; event data may gain fields. Consumers
should ignore unknown input fields. Child lifecycle identifiers combine the
parent session and task IDs; `parent_session_id` and `task_id` identify their
owner. A standalone compaction or session event has an empty `turn_id`.
`session_start` runs once when the native session's first runner is installed
(or before a child's first turn), with `mode` `new` or `resume`. Runner rebuilds
for model/MCP changes do not repeat it. `session_end` runs once on graceful
session close, after child completion for children; crashes cannot guarantee it.
Asynchronous shutdown awaits its bounded owner. Synchronous close starts
cleanup in the background; a closed lease-managed session refuses late hooks.
Startup commands keep an owner if a build waiter is dropped, and closing the
session cancels that startup command.

| Event | Data beyond turn ID | Accepted output fields |
| --- | --- | --- |
| `session_start` | Session metadata above | `decision`, `reason`, `context` |
| `before_prompt` | `prompt` text, `followup` and `has_image` flags | `decision`, `reason`, `context` |
| `before_tool` | Tool identity and arguments above | `decision`, `reason`, `arguments` |
| `after_tool` | Tool identity/arguments, `content`, `is_error`, typed `outcome` | `content` |
| `before_compaction` | `reason`, `focus` | `decision`, `reason`, `context` (retention hint) |
| `after_compaction` | Committed `checkpoint_id`, `reason` | `{}` |
| `turn_end` | `status`: `success`, `error` or `cancelled`; optional error and stop reason | `{}` |
| `session_end` | Session metadata above | `{}` |

Commands must exit zero and emit one JSON response on stdout, or empty output
for a no-op. Diagnostic output belongs on stderr. Unknown response fields,
unsupported fields for an event and malformed JSON are errors.

```json
{"decision":"deny","reason":"Use the approved deployment tool"}
```

`decision` is `allow` or `deny`; omission means continue. `arguments` replaces
the entire argument object before the tool's own validation, child write
policy and authorization. Rewritten arguments are audited as append-only
`otto.hook.arguments` session entries. They cannot change the outer tool name.
`content` replaces only model/display text; original persisted output and
outcome are audited as `otto.hook.result`, and error/effect certainty remain
unchanged. Context and retention hints are labeled as untrusted plugin text.
Later hooks see earlier argument/content rewrites.

Before-hook failure, denial, timeout or invalid output stops that operation.
Tools refused before dispatch settle as `NotStarted`; `after_tool` only runs
for dispatched calls. After-hook failure reports a warning and preserves the
actual operation result; other after hooks still run. After-compaction runs
only for a committed checkpoint, not a no-op. Cancellation/failure still
triggers `turn_end`; after hooks use independent bounded command timeouts.

Timeout defaults to 10 seconds and must be positive and at most 300 seconds.
Before hooks also consume the remaining operation budget. Input is capped at
1 MiB, stdout/stderr at 64 KiB. Known credentials are redacted before stdin
and from responses/diagnostics. Timeout/cancellation kills and reaps the process
group, including descendants. Running package commands check authorization and
resource changes every 500 ms. Unknown effects and hooked calls are never
replayed automatically after a process loss.

### Custom tools

A tool above is registered as `plugin__demo__inspect`; the complete name must
fit 64 characters (package dots become underscores). Input uses standard JSON Schema, validated before process
launch; local schema references work, external HTTP/file references are
unavailable. A custom tool runs through the same authorization registry as
other effectful tools, with the exact arguments, command and package version.
It is never declared replayable.

Its stdin is `{version: 1, event: "tool", workspace, tool, arguments, metadata}`.
`metadata` contains tool-call/operation IDs, attempt and, for authorized native
calls, session/task/workspace scope. It must emit JSON such as:

```json
{"content":"Inspection complete","is_error":false}
```

`content` is required; `is_error` defaults to false. Nonzero exit, timeout,
cancellation or malformed output after dispatch produces an uncertain effect,
without an automatic retry. Use the existing `otto plugin install` and `trust` commands; installation runs
no hook or tool code.

---

Previous: [Automation](automation.md) · [Documentation index](../README.md) · Next: [Troubleshooting](troubleshooting.md)
