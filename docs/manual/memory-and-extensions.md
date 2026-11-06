# Memory and extensions

Previous: [Automation](automation.md) · [Documentation index](../README.md) · Next: [Troubleshooting](troubleshooting.md)

## Memory

Local per-user/per-workspace memory is enabled by default. Use `/remember` to
write a human instruction, `memory_search` to inspect memory, `/memory` for
local management, and `/memory review` for candidates needing a human decision.
Model writes use the shared approval service and cannot manufacture
authorization from retrieved text. Memory never overrides host security,
system rules or the user's actual task.

One memory source is active at a time. `[memory] backend` selects the built-in
file store (`files`, the default) or a plugin package (`plugin:<package>`, see
[plugin memory source](#plugin-memory-source)). `[memory] enabled = false`
turns off memory for either source.

### Memory files

The source of truth is `~/.otto/memory/` (directory `0700`, files `0600`), not a
database or search index. This location is fixed and does not follow storage
routes:

| Path | Contents |
| --- | --- |
| `identity.json` | Store and user scope IDs |
| `records/<id>.md` | Active record |
| `history/<id>/<revision>.md` | Superseded revision of a record, byte for byte as its former record file |
| `tombstones/<id>.json` | Forgotten record ID, without its text |
| `candidates/<id>.json` | Pending proposal |
| `candidates/archive/<id>.json` | Accepted/rejected proposal receipt |
| `.lock` | Exclusive writer lock |

Decided proposals are archived automatically after review. Opening the store
also archives valid decided files left by older versions or an interrupted
archive; pending and invalid files stay in place. Normal recall and pending
review lists do not scan the archive. Exact-ID lookup and explicit history
lists still read archived receipts for approval history and retry confirmation.
Archiving does not delete history or reduce the total file count. Older Otto
versions do not look in the archive for receipts.

Record frontmatter uses one `name: <JSON value>` per line. All sixteen names
below may appear exactly once; the first eleven are required and the five after
`metadata` (`state`, `verified_at`, `review_at`, `superseded_by`,
`state_reason`) are optional on read. A file without them reads as `active`
with no verification or review time. Unknown fields and multiline bodies are
invalid.

```markdown
---
scope: {"namespace":"workspace","id":"project-a"}
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
state: "active"
verified_at: "2026-10-16T10:30:00.000000000Z"
review_at: null
superseded_by: ""
state_reason: ""
---
Reply to technical questions in Chinese.
```

Hand edits apply at the next read, normally the next turn. Deleting a record
file by hand removes a record that was never updated without a tombstone. When
the record has history files, the next time the store opens it writes a
tombstone whose revision is the latest history revision plus 2 and removes the
history directory. Invalid or secret-bearing records are skipped;
`otto memory status` and startup warnings give reasons. Duplicate nonempty
scope/kind/key records resolve to the later `updated_at`; tombstones always
suppress their records. Prefer `/remember` over writing files manually.
Workspace identity is described under [automation](automation.md#workspace-identity).

Writes lock for up to `[memory.turso].busy_timeout` (5 s by default), recheck
revision/key constraints and rename temporary files into place. Revision
checks compare numbers only: a hand edit without a revision increment can be
overwritten by a previously read model update. Two-file operations are not
atomic: a crash may leave a candidate pending after its record was written;
reaccepting can duplicate text (recall deduplicates it). A tombstone left beside
a record still suppresses it.

Every recall scans the records; text is limited to 8192 bytes per record.
Files are **not encrypted at rest**. File tools and the default Bash sandbox
cannot write this out-of-workspace store; explicit unsandboxed access can.

```toml
[memory]
enabled = true
backend = "files"
required = false
recall_tokens = 2000
max_results = 12
require_encryption = false

[memory.turso]
busy_timeout = "5s"

[memory.workspace_ids]
"/canonical/path/to/project" = "stable-id"
```

`required = true` makes an unavailable store fatal rather than disabling memory.
`require_encryption = true` cannot be satisfied by the files backend.
`backend` is `files` or `plugin:<package>`. Restart affected runtimes after
configuration changes.

### Rust memory service extensions

Embedders may inject a `MemoryExtension`; this is not a CLI plugin or an
external service implementation. Keep memory contracts in `otto-core` and
native persistence/access in `otto`. Organization scope is available only to
embedders, through `MemoryExtension::with_organization`. Use the public
embedding API and its tests for the current constructor/interface contract
rather than copying a historical example. An injected extension together with
`backend = "plugin:<package>"` fails startup.

### Plugin memory source

A plugin package provides memory by declaring a `memory` command (see
[memory command protocol](#memory-command-protocol)). User or managed
configuration selects it; project configuration cannot set `[memory]`:

```toml
[memory]
backend = "plugin:otto-mem"
```

Installing or trusting a package does not change the memory source; only
`backend` does. The package must be installed, enabled and trusted. With a
plugin source:

| Consumer | Behavior |
| --- | --- |
| Recall before each request | `recall`; rendered as the request-local memory message within `recall_tokens`; a failure is a warning and the turn continues |
| `memory_search` | `recall`, or `inspect` for an id |
| `remember`, `forget` tools | Registered when the plugin's description declares delegated-learning support (`forget` also needs item targets and suppression) |
| `/remember` | `remember` with origin `human` |
| Finished turns | `observe`, when the description declares it with delegated-learning support; see [turn observation](#turn-observation) |
| `/memory list\|show\|search\|forget\|review`, `otto memory` | Refused with a message naming `plugin:<package>` |
| TUI review panel, ACP memory review, memory advice | Unavailable |

Otto opens no file store under `~/.otto/memory/` and keeps no review queue.
Selecting the plugin in configuration is the learning grant: Otto authorizes
the user and workspace scopes, the input classes `message`, `tool_result` and
`explicit_memory`, and background evolution, so model `remember`/`forget`
calls and observations take effect in the service without an Otto review.
The user scope sent to the plugin is `user/default`; the service identifies
the user from its own credentials. `required`, `recall_tokens`,
`max_results`, `require_encryption` and `workspace_ids` apply;
`[memory.turso]` does not. The package's other hooks and tools run as usual.

Otto builds the source for each runner (new session, resume, `/sandbox
reload`) and runs `describe` once there. When the package is missing,
disabled or not trusted, declares no `memory` command, `describe` fails, or
`require_encryption = true` and the description does not declare encryption
at rest: with `required = true` the runner build fails with that reason;
otherwise Otto writes a warning and the runner has no memory. Revoke and
remove take effect at the next memory call.

Otto ships no client for any service. The [otto-mem plugin](https://github.com/baiyuqing/otto-mem/blob/main/docs/otto-plugin.md)
is one implementation; its installation is documented in the otto-mem
repository. Bind the plugin's token to its package commands in user or managed
config:

```toml
[plugins.bindings.otto-mem]
env = { OTTO_MEM_TOKEN = "${OTTO_MEM_TOKEN}" }
```

#### Memory command protocol

```json
{"extensions":{"dev.otto":{"version":1,"memory":{"command":["${PLUGIN_ROOT}/bin/memory"],"timeout":"10s"}}}}
```

A package declares at most one `memory` object. It counts as an executable
component: it needs trust, and `otto plugin trust` lists it. Each memory
operation runs the command once, with the sandbox, working directory,
bindings, redaction and limits of [package hooks](#hook-protocol-v1): timeout
default 10 s and at most 300 s, further bounded by the operation's remaining
time; stdin 1 MiB; stdout 64 KiB. Stdin is one JSON object:

```json
{"version":1,"operation":"recall","request":{"context":{},"query":"build","limit":12,"token_budget":2000}}
```

Stdout is one JSON object, `{"result":{...}}` or `{"error":{"kind":"unavailable"}}`.

- `operation` is `describe`, which has no `request` and returns a
  `ServiceDescription`, or an operation the description declares: `recall`,
  `observe`, `remember`, `forget`, `inspect`, `feedback`, `prepare`, `apply`,
  `operation_status`, or `reflect`. The `reflect` operation name is part of the
  plugin protocol only; Otto's removed reflection feature does not invoke it.
- `request` and `result` are the JSON forms of the request and result types in
  `crates/otto-core/src/memory/memory_types.rs`, for example `RecallRequest`
  and `RecallResult`, `RememberRequest` and `Receipt`. Unknown output fields
  fail.
- `error.kind` is one of `invalid_input`, `unauthorized`, `unsupported`,
  `conflict`, `rejected`, `unavailable`, `rate_limited`, `outcome_unknown`,
  `canceled`, `closed`. Otto shows its fixed message for the kind, never plugin
  text.
- A non-zero exit, invalid JSON or output over the limit is `unavailable`. A
  timeout, cancellation or revocation after the process started is
  `outcome_unknown` for operations with effects and `unavailable` for
  `describe`, `recall`, `inspect` and `operation_status`.
- Results pass the scope, receipt and content checks Otto applies to every
  memory service.

#### Turn observation

After the `turn_end` hook, Otto sends the turn's messages (the `turn.messages`
set of [hook protocol v1](#hook-protocol-v1)) to `observe` with mode `learn`:

- One observation per `text`, `tool_call` and `tool_result` block. Text
  content is the text; a tool call is the JSON text `{"id","name","arguments"}`;
  a tool result is the JSON text `{"id","name","text","is_error"}`.
- `source.role` is `user`, `assistant`, `context`, `tool_call` or
  `tool_result`; `source.trust` is `external` for tool results and `local`
  otherwise.
- Content is at most 8 KiB per observation; a longer block is split at UTF-8
  character boundaries. `source.id` is `<turn_id>:<message>:<block>:<part>`,
  with indexes from 0.
- A request holds at most 64 observations and 256 KiB of content; a longer
  turn is sent as several requests in order, and the first failure ends that
  turn's observation with a warning.
- Control characters other than newline, carriage return and tab are removed
  from text blocks. An observation that fails the memory content guard, for
  example text holding a `[REDACTED]` marker or a private key delimiter, or
  that is blank, is left out; the others are sent.
- The call runs before the turn returns. A failure is a warning and does not
  change the turn result.

See [hooks and custom tools](#hooks-and-custom-tools) for package bindings.

#### Removed remote backend settings

A `[memory.remote]` table fails startup with:

```text
[memory.remote] was removed: install the external memory service as an Otto plugin, such as otto-mem (https://github.com/baiyuqing/otto-mem), set backend = "plugin:otto-mem" in [memory], bind its token under [plugins.bindings.otto-mem], and delete [memory.remote] from config.toml
```

`backend = "remote"` fails startup with:

```text
memory backend "remote" was removed: install the external memory service as an Otto plugin, such as otto-mem (https://github.com/baiyuqing/otto-mem), set backend = "plugin:otto-mem" in [memory], and bind its token under [plugins.bindings.otto-mem]
```

### Earlier memory databases are not imported

The memory database of Otto 0.4.8 and earlier (`~/.otto/memory/memory.db`) is
not read: the file store starts empty, and `[memory.turso].path` is accepted
and ignored. SQLite cannot read the turso full-text index in that database.
Commit 96646600 is the last build that imports it into `~/.otto/memory/`: run
that build once with all other Otto processes stopped, check the files, then
upgrade. The source database is never modified; delete it yourself.

### Storage operations

Storage routes cover approvals, workflows, schedule, usage, tasks, skill checks
and memory advice; **not** `~/.otto/memory/`. The ordinary binary
provides local SQLite/POSIX storage, not cloud/MySQL/PostgreSQL drivers. Linked
extensions can register domain factories, configured only in user/managed
sources. Secrets use endpoint environment bindings, never inline DSNs/options.
Normal startup opens published namespaces; editing config does not initialize
an empty replacement. Use `otto storage --help` for init/transfer options.

For linked storage callers, SQLite's regular `execute`, `prepare` and query APIs
accept exactly one statement, rejecting stacked SQL and embedded NUL before
live preparation. Validation uses SQLite's trigger-aware boundary recognizer on
scratch state, outside the live connection lock. SQL text is limited to **8 MiB**
and cumulative boundary-recognizer scanning to **64 MiB** per validation; unusually
semicolon-dense literals, comments or trigger bodies may hit the work limit even
below the text limit. `ValidationLimit` reports both limits and recovery in its
visible error message, without echoing submitted SQL. Bind large data values as
parameters or simplify trigger definitions. Only explicitly trusted scripts
should use `execute_batch`, which intentionally permits multiple statements and
does not provide these single-statement guards. Normal single-statement calls
within the limits retain existing behavior, including accepted PRAGMA effects.

Stop every affected process and prevent old binaries restarting before transfer:
local locks cannot fence an uncooperating/distributed writer.

```bash
otto storage transfer "$HOME" /Volumes/otto-state-new transfer-20261019
# Repeat the identical command to resume/reconcile an interrupted transfer.
otto storage abort "$HOME" transfer-20261019
```

Destination parent must exist on a local volume outside source home; roots
must not overlap. Transfer stages/verifies state then publishes one route
generation. Restart using the same source home to follow the published route;
keep source/staging data until destination startup is verified. Memory files
are not copied. Abort restores source admission only before publication and
retains staged bytes/checkpoints; it refuses published/uncertain unreconciled
state. After publication, use another verified transfer to return, preserving
new writes. Extension namespace/config/environment must match publication;
this is coordinated one-host operation, not distributed writer fencing.

For a legacy-schema database of an earlier Otto release,
`otto storage migrate SOURCE DESTINATION` requires Python 3, a new destination in an existing directory, stopped writers,
and preserves source bytes. It reconstructs canonical schemas and imports bound,
typed row values, validating supported schema versions, all table/index definitions
and row counts before publication; source DDL is never executed. Extra columns,
indexes, expressions, triggers or modified constraints are rejected even when table
names match. Previously these modifications could be copied; do not remove them
blindly to force migration. Keep the original database and use its matching older
Otto release to export logical records for a supported transfer, or retain that
release until a compatible migration is available. A failed import does not publish
the destination; retry to a new path after resolving compatibility. Memory
databases are not supported. Do not mix old/new writers.
Read usage peaks without the `sqlite3` tool using
`otto storage window-peaks ~/.otto/usage.db <session-id>` (read-only JSON).

### Local memory use

Recalled record text is request-local and is not copied into session JSONL,
compaction summaries or logs. The IDs and revisions of injected records are
recorded, without text, in the turn's `otto.memory_recall` session entry and in
compaction checkpoints (`memoryRecords`, `memoryCoverage`). A turn or
checkpoint written before these entries existed has unknown coverage.
Standing preferences have their own budget; other active/unexpired
user/workspace records are untrusted reference material, bounded by
`max_results`/`recall_tokens`. Workspace records replace same-kind/key user
records and identical text is deduplicated. Recall is not a complete inventory.

Use `/memory list` or `otto memory list` to page active records (see
[record states](#record-states-and-times)); the current user and workspace
scopes are listed by default, and `--scope user|workspace|all` narrows or
explicitly inventories all stored workspaces. Pass returned `next_cursor` as
`--cursor`. `show` reads an exact record in the selected scopes. Human
`/remember` and `/memory forget <id>` apply immediately. Model writes create
candidates; `/memory review <id> accept|reject` decides pending human reviews.

Every update that replaces a record moves its former revision to
`history/<id>/<revision>.md`. Revision 1 of a record that was never updated has
no history file. Three commands read and use the history, as
`otto memory ...` and as `/memory ...`:

- `history <id>` prints one line per revision, oldest first, with the update
  time, the origin, `restored_from=<n>` when the revision is a restore, and
  `current=yes` on the current one. A history file that cannot be read is
  listed as skipped with its reason.
- `diff <id> <from> [<to>]` compares two revisions; `to` defaults to the
  current revision. It prints the changed fields and a line diff of the text,
  with `- ` for removed lines and `+ ` for added lines. A revision that is not
  in the history is an error.
- `restore <id> <revision>` writes the content of an earlier revision as a new
  revision `n+1` with `source.origin` `human` and the metadata entry
  `otto.restored_from_revision`. It never lowers the revision number. Restoring
  the current revision, a revision that does not exist, or a forgotten record
  fails. A restore is applied immediately and does not go through candidates.

`forget` removes the record file and the whole history directory of the
record, rejects every pending candidate that targets the record (its text is
cleared and the receipt is archived), and writes the tombstone. It then
prints one line per copy of the text it looked for, in one of four classes:

| Class | Meaning |
| --- | --- |
| `removed` | The copy was deleted: `records/<id>.md`, `history/<id>/`, `candidates/<id>.json` |
| `suppressed` | The copy remains but is no longer read |
| `present` | The copy remains and is not deleted |
| `unverified` | Otto did not scan the place, so a copy may exist |

Two places outside the store are always reported. The memory advice audit log
`~/.otto/memory-advice.db` is `unverified`: its rows are keyed by candidate ID
and are not scanned, and a request may hold the record text. Session files are
`present`: recalled text is never written to a session, but user input and tool
output that repeat the text are independent history and are not deleted.

Candidate advice can suggest scope, duplication/conflict and durability, but
never changes authority or accepts anything. By default it uses the session
provider/model; nonempty `TYPESAFE_API_KEY` selects the auxiliary service at
`https://api.typesafe.ai`. Requests contain candidate kind/key/scope/text/reason
and standing user-record keys/texts; refused candidates are never sent.
Exchanges (request/response/outcome/latency, never API key) are retained in
`~/.otto/memory-advice.db`, so protect that file as task content. The 20 s
timeout, invalid reply or service/logging error does not block queuing.
Pending candidates do not expire with their source session.
`otto memory status` reports skipped files/reasons and storage-level pending
counts, which can differ from actionable human approvals. Local startup notices
and connector post-turn cards expose pending reviews; attached clients use
their own refresh paths. ACP snapshot-cursor conflicts require restarting the
page request with an empty cursor, not skipping changed candidates.

### Record states and times

A record has one lifecycle state. A forgotten record is a tombstone, not a
state.

| State | Meaning | Normal recall |
| --- | --- | --- |
| `active` | The record is current | Injected |
| `needs_review` | The record is uncertain or awaits a human decision | Not injected; listed with its state |
| `superseded` | Another active record in the same scope replaces it; `superseded_by` holds that id | Not injected |
| `retracted` | The record was withdrawn or found incorrect; `state_reason` may say why | Not injected |

A record has five times. `created_at` is set once. `updated_at` changes only
when the content changes. `verified_at` is the last explicit human
verification; reading, recall and listing never set it. `review_at` is the next
review deadline: once it has passed, the record leaves normal recall until
`verify` runs. `expires_at` is the existing expiry. A standing preference needs
no deadline; a temporary fact suits `review_at` or `expires_at`.

Two commands change them, as `otto memory ...` and as `/memory ...`:

- `state <id> active|needs-review|retracted [--reason TEXT]` and
  `state <id> superseded <replacement-id> [--reason TEXT]` set the state. The
  replacement must be an active record in the same scope.
- `verify <id> [--review-in AGE]` sets `verified_at` to now, returns a
  `needs_review` record to `active`, and sets `review_at` to now plus `AGE`
  (`30d`, `12h` or `45m`). Without `--review-in` it clears `review_at`. An
  absolute time is not accepted.

`/remember --review-in AGE <text>` sets the deadline when a record is written.
A state change or verification is a new revision with a history file and keeps
`updated_at`. `list` shows only `active` records by default and takes these
filters, which combine:

| Flag | Selects |
| --- | --- |
| `--state active\|needs-review\|superseded\|retracted\|all` | Records in that state; repeatable |
| `--kind K`, `--label L` | Records of that kind or with that label |
| `--updated-after TIME\|AGE` | Content updated at or after the time |
| `--verified-after TIME\|AGE` | Verified at or after the time |
| `--unverified` | Records never verified; not combinable with `--verified-after` |
| `--review-due` | Records whose `review_at` has passed |

`TIME` is an RFC 3339 time and `AGE` counts back from now. The list line prints
`state=`, `superseded_by=` and `state_reason=` when set, then `verified_at=` and
`review_at=`. `memory_search` with a term query returns active records; an id
lookup returns the record with its state.

### Retraction notices

A record that an earlier turn rendered into the model's context may later
leave the active state: superseded, retracted, flagged for review, expired,
updated to a new revision, or forgotten. Its text is still in the
conversation, so before every provider request Otto looks up the current state
of the records the conversation refers to and appends one fixed-template
notice per retired record to the request-local memory message:

```text
<memory_retraction id="A" revision="3" current_revision="4" reason="superseded" replacement_id="B"/>
```

- `id` and `revision` name the record and the highest revision the model saw.
  `current_revision` is the record's current revision when the store reports
  one; for a forgotten record it is the tombstone revision.
- `reason` is one of `needs_review`, `superseded`, `retracted`, `expired`,
  `forgotten` and `updated` (the record is active at a higher revision than
  the one the model saw). `replacement_id` is present for `superseded`.
- The block starts with a fixed preamble: these records are no longer valid
  evidence, a withdrawn record does not imply its opposite, and a named
  replacement is to be used.
- A notice never carries record text, a state reason or a candidate
  explanation. A forget notice holds the id and the reason only.

Notices are selected from the `otto.memory_recall` references of the turns
still in the model view and from the latest compaction checkpoint's
`memoryRecords`, intersected with the current state of those records. They
are not selected by retrieval, and retired text is never re-injected. The same
lookup re-filters the records recalled at the start of the turn before every
provider step, so a forget or a state change inside a tool loop takes effect
at the next request. A turn or checkpoint written without references (unknown
coverage) contributes no notices, and neither does a record whose text entered
the conversation only as a paraphrase, tool output or user text. A plugin
memory source produces notices only for records it still returns by id; it
cannot report a forgotten record. Notices are request-local: they are not
written to the session file, compaction summaries or logs. `/context` shows
the notices rendered for the previous turn's last request.

### Standing preferences

For habits that apply in every workspace, use user scope, `preference` or
`instruction` kind, and a key:

```text
/remember --scope user --kind preference --key plan-first Explain the plan before executing a task.
```

Human `/remember` writes immediately; model writes queue candidates for shared
approval. Unkeyed user preferences/instructions are rejected.
Standing records are recalled every turn within a 2000-estimated-token budget.
An over-budget write/accept fails; the candidate stays pending. Forget an old
record with `/memory forget <id>`, then accept again. `otto memory status` shows
usage; `/context` shows the actual previous-turn injection. Existing oversized
sets inject oldest first until the first record that does not fit and warn
about omitted records.


## Skills

Skills are directories containing `SKILL.md` (YAML frontmatter plus Markdown)
in the Agent Skills format. `make install` installs bundled skills globally.
The checkout bundles `sandbox-setup` for sandbox recovery and
`rust-best-practices` for repository-aware Rust implementation and review.

```toml
[skills]
enabled = true
typesafe_skill_check = true
paths = ["~/.otto/skills", ".otto/skills"]
disabled = []
```

Later roots win name conflicts. Names equal directory names (`a-z`, digits,
hyphen; 1..64 chars), descriptions are 1..1024 chars. Invalid skills warn and
are skipped. Existing roots become Seatbelt read paths at startup; restart
after installing new roots. Skill file reads remain canonical-path confined.

Use `/skill` to browse, `/skill <name>` for details, and `/skill set <name>
enabled|disabled` to maintain the disabled list. TUI Space toggles, Enter shows
details, Esc returns. Disabled skills remain on disk but disappear from model
listing, tool loading and skill-derived agents after restart.

The model loads instructions/support files with `skill`. The prompt listing is
bounded to 8 KiB, progressively shortening entries before omitting names; stderr
warns about omissions. A listed skill is instructions, not permission.

Optional paired `input`/`output` frontmatter (each 1..1024 chars) declares a
contract and registers a skill-derived agent, marked `exec="agent"`. Invalid
pairs warn and are ignored without disabling the skill. Delegation gives fresh
context and returns only results; inline loading remains available. Skills
cannot choose extra tools/model/write authority. Explicit same-name `AGENT.md`
wins; disabled agents prevent registration.

### Automatic contract check

When skills/checking are enabled and `TYPESAFE_API_KEY` is nonempty, the first
delegation to each contract skill version sends full `SKILL.md`, including
frontmatter, to `https://api.typesafe.ai` to review self-containment/contract
quality. Locally flagged patterns are judged without sending. Append-only
results in `~/.otto/skill-checks.db` appear in skill details. Disable with
`[skills].typesafe_skill_check = false`; without the key or when disabled,
nothing is sent and the database is not opened. Treat this as external data
sharing, not a safety guarantee or approval grant.

## MCP servers

MCP connects local **unsandboxed stdio processes** or Streamable HTTP servers.
Inspect and trust executables/destinations before connecting. Project executable
authority is separately granted with `otto trust DIR --executables`; admission
alone is insufficient. Store credential bindings, not literal secrets:

```bash
otto mcp add docs --transport http --url https://mcp.example.com/mcp \
  --header 'Authorization=Bearer ${DOCS_MCP_TOKEN}'
otto mcp add local --transport stdio --command python3 --arg /trusted/server.py
otto mcp list
otto mcp disable local
otto mcp enable local
otto mcp remove local
```

`--env KEY=ENVVAR` stores `KEY = "${ENVVAR}"`, not its current value. Commands
write the user source; restart to apply changes. OAuth HTTP servers use
`--auth oauth`, then `otto mcp login <server>` (or `/mcp login <server>`) and
restart; logout removes the stored token.

```toml
[mcp]
enabled = true
call_timeout_secs = 60
connect_timeout_secs = 20

[mcp.servers.docs]
transport = "http"
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_MCP_TOKEN}" }

[mcp.servers.local]
transport = "stdio"
command = "python3"
args = ["/trusted/server.py"]
cwd = "."
env = { ACCESS_TOKEN = "${SERVICE_TOKEN}" }
```

Names match `^[A-Za-z0-9_-]{1,32}$`. `${VAR}`/`${VAR:-default}` expand at
startup in env/headers/URL/command/args/cwd; unset without default fails. Only
env/header substitutions are registered as secrets. Their redaction limits are
64 values, 8 KiB each, 16 KiB total; exceeding these fails that server. Stdio
inherits only configured env plus `PATH`, `HOME`, `TMPDIR`, `LANG`, `TERM` and
runs outside Seatbelt. OAuth tokens live in `~/.otto/auth/mcp/<server>.json`.

`/mcp` shows connecting/connected/disabled/needs-login/failed states and protocol
era. Terminal startup is usable before connections finish; router tools swap
between turns after all settle. Headless/server startup connects before the
first turn. One failed server does not remove other tools. Connection outcomes
and name deduplication apply in config order, not completion order.

Models search `mcp_search_tools`, then call `mcp_call_tool` with the returned
full `mcp__<server>__<tool>` name; schemas are lazy, not all injected into context.
Tool-name invalid bytes become underscores; names over 64 bytes or collisions
are skipped with warnings. Calls use normal authorization. Text resources are
returned; binary image/audio/resources become placeholders, not embeddings.
Call timeout cancels on the server but cannot prove external effects did not
occur. Diagnose failures with `/mcp` and [Troubleshooting](troubleshooting.md).

## Plugin packages

Plugins package Skills/MCP using Agent Plugins 1.0, with limited
`.claude-plugin/plugin.json`/`.codex-plugin/plugin.json` adapters. No installed
packages means no added capability. A portable package contains `plugin.json`,
`skills/<name>/SKILL.md`, optional `mcp.json`, and its server/scripts. A runnable
[hook demo](../../examples/plugins/hook-demo/README.md) covers extension commands;
the [TokenEconomizer bridge](../../examples/plugins/tokeneconomizer/README.md)
runs an external hook host from a package.

`claude` packages' Claude Code hooks run natively (see
[Claude Code hook format](#claude-code-hook-format)); `codex` packages' hooks
are still unsupported.

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

Command handlers require their interpreters/dependencies; built-in Lua handlers
need no external interpreter. Installation never runs npm/pip scripts. Git sources
require full commit IDs, not tags/branches. Installer
subprocesses exclude hooks, submodules, global/system filters/config and injected
Git config overrides; SSH agent/configured askpass authentication is available.
URLs with passwords, HTTPS usernames, query or fragment are rejected.

Installation snapshots content in `~/.otto/plugins/packages/`. Skills activate
without executable trust; MCP/hooks/tools require **explicit digest-bound
trust**. Inspect before trusting. Changes, including executable modes, require
reinstall/retrust. MCP stdio remains unsandboxed; HTTP trust authorizes the
connection. Remote servers use `streamable-http`, HTTPS except loopback;
legacy HTTP+SSE is unsupported.

Enable/disable applies at next runner build (normally new session/restart).
Bash can read plugin snapshots on macOS because `~/.otto/plugins` is not on the
[sandbox read denylist](safety-and-tools.md#a-sandboxed-command-reads-every-path-except-the-read-denylist).
Revoke/remove immediately blocks subsequent MCP/command use and closes servers;
it does not undo effects. Removal retains snapshots and
`~/.otto/plugins/data/<name>/`; no automatic garbage collection exists. Replace
a name by removing then installing. Skills appear as `package:skill`; inspect
shows bounded internal MCP IDs.

## Hooks and custom tools

Installed `plugin.json` uses `extensions.dev.otto` version 1 for hooks, tools
and a [memory command](#memory-command-protocol):

```json
{
  "name": "demo",
  "version": "1.0.0",
  "extensions": {
    "dev.otto": {
      "version": 1,
      "hooks": {
        "before_tool": [{"matcher": ["bash"], "command": ["python3", "${PLUGIN_ROOT}/check.py"], "timeout": "10s"}]
      },
      "tools": [{
        "name": "inspect",
        "description": "Inspect project metadata",
        "command": ["python3", "${PLUGIN_ROOT}/inspect.py"],
        "input_schema": {"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}
      }]
    }
  }
}
```

Commands are argv arrays, not implicit shells. `${PLUGIN_ROOT}`/`${PLUGIN_DATA}`
refer to the snapshot/data directory. Commands start in the workspace through
the current sandbox/filtered env, without new filesystem grants. A package's
commands also receive `PLUGIN_ROOT` and `PLUGIN_DATA` as environment variables
(standalone hooks do not) and the variables bound to that package name in user
or managed configuration:

```toml
[plugins.bindings.demo]
env = { DEMO_TOKEN = "${DEMO_TOKEN}" }
```

Values use the MCP binding template rules and literal credentials are
rejected. A package key accepts only `env`; `headers`, `auth` and OAuth fields
are MCP-only (`plugin:server` keys). Bound values are redacted from hook
input/output and model-visible text. Bash, standalone hooks and other packages
do not receive them. An unresolvable package binding fails the runner build.
Only explicit sandbox `off` is unconfined; Linux requires that choice to run
commands. Hooks cannot request elevation or grant authorization. Untrusted
packages omit commands with a diagnostic. Enable/disable affects new runners;
revoke/remove blocks existing snapshots without replaying effects.

Standalone user/trusted-project hooks use TOML:

```toml
[[hooks.before_tool]]
matcher = ["bash"]
command = ["python3", ".otto/hooks/check.py"]
timeout = "10s"
```

Project hooks/files require content-bound `otto trust DIR --executables`.
Referenced workspace files are snapshotted; changes require rebuilding the
runner and renewed trust when applicable. Higher config layers replace an
event's array. Standalone hooks run first, then packages sorted by name, each
in declaration order. Native CLI/TUI/HTTP/ACP, children and compaction share
this path. Matchers use exact tool names or one `*`; omission matches all.
MCP matching uses selected remote names; router arguments stay `{tool, arguments}`.

### Claude Code hook format

A `claude` package (`.claude-plugin/plugin.json`) runs its Claude Code
`command` hooks natively. `codex` packages, hooks in Claude settings files
(`~/.claude/settings.json`, `.claude/settings*.json`) and the portable
`extensions.com.anthropic.hooks` field are not read; they are follow-up work.
Trust, sandbox, redaction and limits are those of Otto hooks:
`otto plugin inspect <name>` lists the hooks under `claude_hooks`, and
`otto plugin trust <name>` prints `Claude hooks: <json>` before authorizing.
A `claude` package trusted before this support existed runs its hooks after
the upgrade; there is no re-trust step.

**What is read.** `hooks/hooks.json` (an object with a `hooks` key and an
optional `description`) and the manifest `hooks` field, which is a path string
inside the package, an inline object, or an array of both. Only
`type: command` handlers run. Each handler accepts `matcher` (on its group),
`timeout` in seconds, `shell` (`sh` or `bash`), `args` and `async`. Handlers
with another `type`, an `if` condition or another shell are skipped with a
diagnostic. `once`, `onFailure` and `statusMessage` are ignored. `async: true`
runs synchronously with a diagnostic.

**Events and matchers.** One Otto event produces at most one Claude event.

| Claude event | Otto event | Matcher value |
| --- | --- | --- |
| `SessionStart` | session start; `source` is `startup` or `resume` | `source` |
| `SubagentStart` | session start of an Otto subagent session | `agent_type` (`otto-subagent`) |
| `UserPromptSubmit` | before prompt | not used |
| `PreToolUse` | before tool | tool name |
| `PostToolUse` | after tool, no error | tool name |
| `PostToolUseFailure` | after tool, error | tool name |
| `Stop` | turn end, status `success` | not used |
| `SubagentStop` | turn end of an Otto subagent session | `agent_type` |
| `PreCompact` | before compaction; `trigger` is `manual` or `auto` | `trigger` |
| `PostCompact` | after compaction | `trigger` |
| `SessionEnd` | session end; `reason` is always `other` | `reason` |

A matcher of `*` or empty matches everything, `A|B` is a list of exact names,
and any other text is a regular expression. A configuration that names
`Notification`, `PermissionRequest`, `PermissionDenied`, `FileChanged`,
`CwdChanged`, `InstructionsLoaded`, `ConfigChange`, `WorktreeCreate`,
`WorktreeRemove`, `TeammateIdle`, `TaskCompleted`, `TaskCreated`,
`Elicitation` or `ElicitationResult` gets the diagnostic
`hook event <Event> never fires in Otto`.

**Payload.** The stdin JSON holds `session_id`, `transcript_path`, `cwd`,
`hook_event_name` and `prompt_id`, plus the event fields; subagent sessions add
`agent_id` and `agent_type`. `transcript_path` is Otto's session JSONL file,
not the Claude transcript format. `permission_mode`, `effort` and
`scratchpad_dir` are not sent. Tool names and inputs are translated, and
`updatedInput` is translated back:

| Otto tool | Claude `tool_name` | Input changes |
| --- | --- | --- |
| `bash` | `Bash` | none |
| `read`, `write` | `Read`, `Write` | `path` becomes absolute `file_path` |
| `edit` | `Edit` | `file_path`; `old_text`, `new_text` become `old_string`, `new_string` |
| `grep` | `Grep` | absolute `path`; `ignore_case` becomes `-i` |
| `find` | `Glob` | absolute `path` |
| `mcp_call_tool` | the MCP tool's own name | `tool_input` is the call arguments |

**Responses.** The exit code and the JSON on exit 0 are handled per event.

| Result | Effect |
| --- | --- |
| Exit 2 on `PreToolUse`, `UserPromptSubmit`, `PreCompact` | blocks; stderr is the reason |
| Exit 2 on `PostToolUse`, `PostToolUseFailure` | stderr is appended to the tool result |
| Exit 2 on `SessionStart`, `SubagentStart`, `Stop`, `SubagentStop` | stderr is shown as a notice |
| Other nonzero exit | notice `hook exited <code>: <stderr>`; does not block |
| `continue: false` | blocks on `PreToolUse`, `UserPromptSubmit`, `PreCompact` with `stopReason` as the reason; a notice elsewhere |
| `systemMessage` | notice |
| `permissionDecision` `allow` or `deny` | applied; `ask` and `defer` are denied with a notice |
| `updatedInput`, `updatedToolOutput` | rewrite the tool input or output |
| `additionalContext` | context on `UserPromptSubmit`, `SessionStart`; appended to the tool result on `PostToolUse`, `PostToolUseFailure`; dropped on `PreToolUse` |
| `decision: block` | deny on `UserPromptSubmit`, `PreCompact`; appended to the tool result on `PostToolUse`; a notice on `Stop`, `SubagentStop`, because Otto does not continue a turn |

Plain-text stdout is context on `UserPromptSubmit` and `SessionStart` only. A
timeout or a signal fails like an Otto hook: the action fails on
`SessionStart`, `UserPromptSubmit`, `PreToolUse` and `PreCompact`. Claude Code
treats a timeout as non-blocking; Otto does not.

**Execution.** The command runs as `/bin/sh -c <command>` (`/bin/bash` when
`shell` is `bash`); with `args`, the command and arguments are executed
directly. `${CLAUDE_PLUGIN_ROOT}`, `${CLAUDE_PLUGIN_DATA}` and
`${CLAUDE_PROJECT_DIR}` are substituted. The environment adds
`CLAUDE_PLUGIN_ROOT`, `CLAUDE_PLUGIN_DATA`, `CLAUDE_PROJECT_DIR` and
`CLAUDE_CODE_SESSION_ATTENDED` (`0` for `otto --prompt`, else `1`) next to
`PLUGIN_ROOT` and `PLUGIN_DATA`. The default timeout is 60 seconds, 30 for
`UserPromptSubmit` and 10 for `SessionEnd`; larger values are clamped to 300.
The sandbox, redaction, 1 MiB input and 64 KiB output limits are those of
Otto hooks. Hooks run sequentially, after the package's `dev.otto` hooks.

**Diagnostics.** Skipped handlers are reported by `otto plugin install` and
`otto plugin inspect`: unsupported hook type, condition or shell, invalid
timeout or matcher, and events that never fire. The diagnostics
`unsupported component hooks` and `unsupported hooks at hooks/hooks.json` no
longer appear for `claude` packages; `codex` packages still get them.

### Hook protocol v1

Each command receives one stdin JSON object followed by EOF:

```json
{"version":1,"event":"before_tool","workspace":"/project","headless":false,"session":{"session_id":"s1","parent_session_id":null,"task_id":"","mode":"new","history_path":"/home/u/.otto/sessions/<key>/s1.jsonl"},"data":{"turn_id":"op_1","tool":"bash","tool_call_id":"call1","operation_id":"op_1","attempt":1,"arguments":{"command":"git status"}}}
```

Ignore unknown input fields. Session metadata identifies child ownership;
standalone session/compaction events have empty turn IDs. `session_start` runs
once at first runner installation, not every rebuild. `session_end` is once on
graceful close, not guaranteed on crash. Closing cancels startup commands and
refuses late hooks; asynchronous shutdown awaits bounded cleanup.

| Event | Data beyond turn ID | Accepted response fields |
| --- | --- | --- |
| `session_start` | Session metadata | `decision`, `reason`, `context`, `notice` |
| `before_prompt` | `prompt`, `followup`, `has_image` | `decision`, `reason`, `context`, `notice` |
| `before_tool` | Identity/arguments above | `decision`, `reason`, `arguments`, `notice` |
| `after_tool` | Identity/arguments, `content`, `is_error`, typed `outcome` | `content`, `notice` |
| `before_compaction` | `reason`, `focus` | `decision`, `reason`, `context` retention hint, `notice` |
| `after_compaction` | Committed `checkpoint_id`, `reason` | `notice` |
| `turn_end` | `status`: `success`, `error`, `cancelled`; optional error/stop reason; `turn` or `turn_omitted` | `notice` |
| `session_end` | Session metadata | `notice` |

`session.history_path` is the session file path read at each call. It is empty
while a lazily created session has no file, as in the `session_start` of a new
session. `headless` is true for `otto --prompt` runs only. `notice` is shown to
the user as a notification and is never sent to the model. The notices of
several hooks for one event are joined with newlines, and a denying hook's
notice is kept with the earlier ones. A `session_end` notice is not displayed
because that event has no output sink.

Exit zero and emit one JSON object, or no stdout for no-op; diagnostics go to
stderr. Unknown/unsupported response fields and malformed JSON fail. `decision`
is allow/deny; omitted means continue. `arguments` replaces the entire object,
not outer tool name, before validation/child ownership/authorization. Rewrites
are audited as append-only `otto.hook.arguments`. `content` changes only
model/display text: original output/outcome is audited as `otto.hook.result`,
and error/effect certainty stays unchanged. Context is labeled untrusted.
Later hooks see earlier rewrites.

Before-hook failure/denial/timeout stops dispatch (`NotStarted`); no `after_tool`
runs for refused tools. After-hook failure warns and preserves actual results;
remaining after hooks still run. After-compaction requires a committed
checkpoint. Failure/cancellation still invokes bounded `turn_end`.

`turn_end` data carries `turn.messages`: the messages appended during the run,
in order, including messages a compaction in the same run removed from the
model context. Each message has `role` (`user`, `assistant`, `tool`,
`context`) and `blocks`:

```json
{"turn":{"messages":[{"role":"user","blocks":[{"type":"text","text":"fix the test"}]},{"role":"assistant","blocks":[{"type":"tool_call","id":"call1","name":"bash","arguments":{"command":"cargo test"}}]},{"role":"tool","blocks":[{"type":"tool_result","id":"call1","name":"bash","text":"ok","is_error":false}]}]}}
```

Image and reasoning blocks are omitted. When the complete input would exceed
1 MiB, `turn` is replaced by `"turn_omitted":"size"` and the hook still runs.
Child-agent turns have a non-null `session.parent_session_id`.

Timeout is positive, at most 300 s, default 10 s; before hooks also consume the
operation budget. Input cap 1 MiB, stdout/stderr 64 KiB. Known secrets are
redacted both ways. Cancellation/timeout kills and reaps the process group.
Running package commands recheck authorization/resources every 500 ms. Hooked
calls and unknown effects are never automatically replayed after process loss.

### Lua handlers

Otto includes Lua 5.4 by default, including in source/install builds. An installed
package's `dev.otto` hook or custom tool may use `"lua": "scripts/handler.lua"`
instead of `command`. Choose exactly one handler: a nonempty `command` argv or a
package-relative `.lua` source path (no absolute paths or `..`). Existing command
packages keep working; their serialized registry entries omit the new `lua` field.
Unknown fields are rejected. Older Otto binaries cannot read packages that use
the new Lua field: remove those packages before downgrading. Standalone TOML hooks
and memory backends do not accept Lua handlers.

See the installable [Lua example](../../examples/plugins/lua-demo/README.md).
Install, inspect and explicitly trust its exact digest, then start a new session
or restart long-lived runtimes. Lua tools are discovered as `plugin__<package>__<tool>`
through the same parent/child registry as command tools, with the declared input
schema and normal tool approval. Hook responses may deny, never grant approval.
Editing the original source does not change installed snapshots. To update,
remove/reinstall, inspect/retrust the new digest and restart affected runtimes.
Revoke/remove blocks existing runners and cooperatively stops running Lua calls;
disable only changes subsequent runner builds.

A Lua chunk receives the existing protocol object as its first argument:

```lua
local input = ...
return {content = tostring(input.arguments.number * 2), is_error = false}
```

Return a Lua value matching the existing hook/tool JSON response protocol; no
stdin/stdout parsing is needed. JSON objects map to string-keyed tables, arrays
to consecutive integer-keyed tables and `null` to `json.null`. Use `json.array({})`
for an empty array (an ordinary empty table returns `{}`). Cycles/shared tables,
sparse/mixed keys, functions, non-finite numbers and invalid UTF-8 fail the call.

Each call uses a fresh VM with only restricted base functions and `math` plus
`json.null`/`json.array`. No `io`, `os`, environment, files, processes, network,
package/module manager, `require`, debug/FFI, dynamic code loading, metatable
mutation, protected calls, coroutine, string or table libraries, or Otto tool
calls are available. Package credential environment bindings are not passed to
Lua. Pure Lua spawns no process and does not need `--sandbox off`; command
handlers retain the existing shell sandbox path.

Limits: source text 256 KiB (UTF-8 only, no bytecode); redacted input 1 MiB;
input/output depth 64 and 8192 nodes; encoded output 64 KiB; Lua allocations
16 MiB; 2 million Lua instructions. `timeout` uses the existing default 10s and
maximum 300s, additionally bounded by the operation deadline. Cancellation,
revocation and deadlines are **cooperative**, checked during Lua instructions
and at phase boundaries. Parsing, setup/destruction and bounded host conversion
are not hard-preemptible. This is an in-process capability-restricted VM, **not
OS isolation**; native engine vulnerabilities remain a risk. See the
[security contract](../development/lua-plugin-security.md).

Missing/escaping/oversized/binary sources and conflicting handlers fail install.
Syntax/runtime/resource-limit errors fail calls using bounded, non-echoing error
categories. Check source syntax, available functions, response fields and limits;
fix and reinstall/retrust/restart. Do not disable security to recover. Existing
error policies still determine whether hook failure blocks the operation.

### Custom tools

The example registers `plugin__demo__inspect` (complete name ≤64 chars; package
dots become underscores). JSON Schema validates before launch; local references
work, external file/HTTP references do not. Authorization binds exact args,
command/Lua handler and package digest. Custom tools are never replayable.

Stdin is `{version: 1, event: "tool", workspace, tool, arguments, metadata}`;
metadata includes tool-call/operation IDs, attempt and authorized native scope.
Emit `{"content":"Inspection complete","is_error":false}`. `content` is
required, `is_error` defaults false. Nonzero exit, timeout, cancellation or
invalid output is an error; after possible dispatch, do not assume no effects.
