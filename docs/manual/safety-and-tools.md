# Safety and tools

## Runner identity

The built-in read-only `session_info` tool takes exactly `{}` and returns only
`{"session_id":"..."}`. It reports the executing runner's own runtime identity,
not a task ID: main runners use their session header ID; each child has its own
ID, matching its transcript header when persisted. Resuming an interrupted child
retains that child's existing ID. With `--no-session`, main and child IDs remain
stable for that runtime but do not imply a saved or resumable session. The tool
exposes no paths, history or credentials and needs no approval or configuration.
It is enabled by default; named child tool allowlists still apply. Rebuild/reinstall
and restart Otto to make a newly added built-in available in an existing process.

Previous: [Interactive use](interactive-use.md) · [Documentation index](../README.md) · Next: [Servers and connectors](servers-and-connectors.md)

Approval titles, reasons, proposal details, reviewer context/reasons and decision
feedback are not secret-scanned, including credential-shaped text, redaction
markers and configured exact secret values. This content may be displayed and
persisted: **do not include actual secrets**. Approval policy, permissions and
human checkpoints still apply; model redaction and memory secret
protections still apply. No extra configuration is needed.

## Unified approvals

Otto proceeds within the task you requested. It asks when a material choice is
missing, the scope changes, or you requested a checkpoint. Your original task
provides the authorization evidence; accepted instructions and standing memory
versions constrain its scope. Assistant replies, tool output and generated
sub-agent instructions cannot grant new authority.
Children inherit their root session's current settings.

Sandbox permissions and approval settings are independent. Ordinary confined
work runs directly. Additional authority creates an exact, version-bound request
in the shared journal. A session has an approval policy (`on_request` or `never`)
and a reviewer (`user`, `auto_review` or `bypass`). The familiar modes are presets:

| Mode | Policy / reviewer | Behavior |
| --- | --- | --- |
| **MANUAL** | `on_request` / `user` | Ask you for additional authority. |
| **AUTO** (default) | `on_request` / `auto_review` | Review eligible operations against the task and exact targets. |
| **YOLO** | `on_request` / `bypass` | Approve requests owned by this session without review. |

AUTO reviews shell escalations, sandboxed shell commands that change external
state ([classified commands](#classified-commands-wait-for-approval-inside-the-sandbox)),
external MCP calls, memory/skill writes, ordinary
model configuration changes, schedules and cross-workspace delivery. Task-related low or
medium risk operations can proceed; ordinary persistent writes need evidence
covering their contents and scope. High risk needs exact user authorization and
bounded targets. Critical or explicitly forbidden operations are denied. An
ambiguous choice, missing evidence, an oversized review or a reviewer failure
asks you. Three consecutive review denials stop further escalation attempts in
the logical task; Otto must use a safer path or obtain concrete user direction.

User questions, explicit approval checkpoints, workflow human gates and security
authority changes still require you in AUTO. A previously authorized operation
does not need the same confirmation again. The model cannot invent a new choice.
User/managed permission rules are checked first: deny takes precedence over ask,
which takes precedence over allow. Otto parses the command and checks each
simple command in it. An `allow` command-prefix rule applies when every
command joined by `&&`, `||`, `;` or `|` starts with the prefix and the
program uses no expansions, substitutions, control flow or redirects other
than `2>&1` and output to `/dev/null`; anything else requires review. A `deny` or
`ask` command-prefix rule applies when any command starts with the prefix,
so `["git", "push"]` also matches `git push origin "$BRANCH"`.

User or managed configuration can set defaults in `[approval]` and select a
configured model with `reviewer_model` (a `<provider>/<model_id>` selector;
see [provider and model configuration](getting-started.md#providers-and-models)). Without an override, the
reviewer uses the root session's provider/model. Rules select exactly one of
`source`, `tool` or `command_prefix`; their decision is `allow`, `ask` or `deny`.
Project configuration, skills, model messages and tool output cannot install
these rules. For example, in the user config:

```toml
[approval]
policy = "on_request"
reviewer = "auto_review"

[[approval.rules]]
tool = "bash"
decision = "ask"
```

Only requests needing your decision appear in pending queues, approval cards and
panels. A queued or running review is recorded as a noninteractive status.
Approval and application remain separate: an approved operation can still fail,
and an operation with an unknown external effect is never replayed automatically.

Each tool implementation declares its own retry safety; model arguments and
configuration cannot relax it. Only genuinely read-only tools qualify for
bounded automatic recovery replay. Replacing the full plan is idempotent but
is not automatically replayed. Writes, shell commands, web requests, approval
queue changes, `agent_yield` and `submit_verification` are conservative
non-idempotent operations; custom, plugin and MCP tools default to the same
safe classification. When an effectful call is interrupted or its effect is
uncertain, Otto settles an interrupted/unknown-effect result into the model's
history instead of automatically retrying the invocation. Inspect the target
state before trying again. The model may choose a **new invocation** with a
fresh call ID, including the same arguments; that is a new decision, not a
retry of the interrupted operation. Existing approval and sandbox requirements
still apply, and permission to execute does not authorize automatic replay.

**YOLO** is the deliberate answer to "stop asking me": the session decides
every request that carries its id, including plan reviews, sandbox
escalations and durable grants, and including requests made
by its own child tasks. Choose it with `/permissions yolo` or `--yolo`. It is a
blanket consent: a request you would have refused is approved anyway, so the
recorded decision names `YOLO`, the frontends always show the mode, and one
command (`/permissions manual` or a mode button) takes it back — including
for requests that are already waiting.

What no mode changes:

- workspace-wide requests (no session id) and another session's requests are
  never decided by this session's mode;
- only you can set the mode. No tool argument, model request or chat
  participant can select YOLO; the model cannot read or write the mode;
- argument and version validation, the recorded journal, the application
  outcome, the sandbox boundary and recovery rules are unchanged. Approval is
  still not execution: a failed or unknown application is reported as such.

`/permissions` shows the mode, the policy, the reviewer and pending approval
counts. `/permissions manual` requires manual approval, `/permissions auto`
selects the default, and `/permissions yolo` decides everything. These presets
reset both policy and reviewer. `/permissions policy on_request|never` changes
only the policy, and `/permissions reviewer user|auto_review` changes only the
reviewer. `/auto [args]` is an unlisted alias of `/permissions [args]` and also
accepts the older `on` (AUTO) and `off` (MANUAL). `never` rejects
requests needing additional authority without prompting, while confined work
continues and the sandbox stays enabled. The reviewer is inactive under `never`.
The preference survives session loading and process
restarts in `approvals.db`; new sessions start in AUTO. TUI and Web footers show
MANUAL, AUTO or YOLO. The same commands work in ACP. Telegram and Feishu
accept `/permissions`, `/permissions manual|auto` and the `policy`/`reviewer`
forms; they reject `/permissions yolo`, so a chat participant cannot select
YOLO. In Telegram, `/permissions` without arguments shows AUTO/MANUAL buttons
that apply to the session shown on the message; a YOLO session reports YOLO in
the message and `/permissions manual` leaves it. After a successful selection,
the message shows the resulting mode and removes the buttons. Send
`/permissions` again to open fresh controls. Clicking a mode button updates that message instead of sending
another card.

The session API exposes `GET /v1/sessions/{id}/approval-mode` and `PUT` with
`{"automatic":true|false}` or `{"mode":"manual"|"auto"|"yolo"}`, returning
`{"automatic":boolean,"mode":"..."}`. Legacy clients see `never` as AUTO with
`automatic:true`; read the canonical policy to distinguish it. A body that contradicts itself is refused. ACP
clients use `_otto/approval_mode/get|set` with `sessionId` and, for set,
`automatic` or `mode`, and read the same two fields.
`GET /v1/sessions/{id}/approval-events?after=<cursor>` returns ordered recorded
requests and decisions, including review metadata. Advanced clients use
`GET /v1/sessions/{id}/approval-policy` and `PUT` with
`{"expected_revision":1,"policy":"on_request","reviewer":"auto_review"}`.
The response includes `policy`, `reviewer`, `revision`, legacy `mode` and
`automatic` labels, and `reviewer_active` (false under `never`).
ACP exposes `_otto/approval_policy/get|set` with `sessionId`, and the setter
requires `expectedRevision`, `policy` and `reviewer`. Updates reject stale revisions; refresh settings before retrying.

Before database upgrades, stop all Otto/server/connector writers, install with
`make install`, then restart. Mixed-version writers are unsupported. Approval
schema upgrades back up the database beside the source; old pending requests
remain human-required. Downgrades need a compatible pre-migration backup with
all writers stopped. Preserve the newer journal separately: restoring a backup
loses later decisions. `/permissions manual` disables automatic review without a
restore.


Otto uses one approval service for tool permissions, memory review, workflow
gates, generated skills and model configuration changes. Pending requests survive in
`~/.otto/approvals.db`. A decision and the outcome of applying it are separate:
approving an operation does not mean it has succeeded.

The model knows the `request_approval` tool and decides when it needs your
confirmation of a concrete plan or approach. The tool waits for your decision
and optional feedback. Later tool calls in that same batch do not run; the model
must consider your answer first. A plan confirmation gives no additional
sandbox or external-tool permission. Mandatory domain checks still apply.

- TUI: one centered approval panel is shown at a time over the conversation.
  **Approve** is selected on the left by default and **Deny** is on the right;
  use `←`/`→`, Enter, or y/n. The read-only Document area does not take focus,
  including when clicked. The TUI has no feedback field. A successful Approve
  or Deny closes the panel immediately and advances to the next pending request.
- Passive chat delivery is separate from approval visibility. An approval made
  during a Telegram or Feishu turn is presented only to that exact originating
  chat. Workspace-visible memory reviews remain discoverable from authorized
  approval or `/memory` lists, but an originless or unrelated request is not
  broadcast to every chat bound to the session or workspace.
- Web: the approval queue shows the full proposal, feedback input, approve,
  reject and withdraw controls. It refreshes pending state across clients.
- Telegram/Feishu: use the request's card buttons, or
  `/allow <request-id> [feedback]` and `/deny <request-id> [feedback]`.
  With multiple requests, select an explicit ID.

The same request closes on all connected surfaces after a decision. Old
buttons cannot approve a new request. Ordinary messages while a foreground
request waits use a restricted discussion; they do not silently approve it.
Memory/workflow reviews remain independent of the foreground turn.

Generated skills still pass the rule scan and configured model review, then
wait for human approval: YOLO decides the review, and the rule scan, the
configured model review and the artifact checks still run. Saving a model
configuration change still requires a restart.
Read-path requests explicitly disclose persistence; commands remain sandboxed.

## Provider egress secret and PII guard

Sandboxing controls what an agent can access; it does not by itself control what
an authorized read may send to a remote model. Before every provider call, Otto
therefore inspects an ephemeral copy of the complete request locally. This
boundary covers ordinary turns, local and remote context compaction, sub-agents,
and one-shot model calls used by approvals, advice and memory.

The guard first removes credential values Otto already knows from its runtime
configuration. It also recognizes a conservative set of high-confidence secret
forms, including private-key blocks, authorization credentials, sensitive
configuration assignments and strongly identified token formats. Detection is
deterministic local code: suspect text is not sent to another model for
classification.

The always-on PII pass recognizes ASCII email addresses, international phone
numbers beginning with `+`, payment-card numbers from known card networks that
pass the Luhn checksum, and Chinese resident identity-card numbers with valid
dates and checksums. In free text and JSON values it replaces each distinct
value with a typed placeholder such as `<EMAIL_1>` or `<PAYMENT_CARD_1>`; the
same value keeps the same placeholder throughout one request. There is no PII
configuration or remote classifier.

Free text and JSON strings that can be changed without altering their meaning are
replaced before transmission. If a credential or recognized PII appears in a
structural or opaque field—such as a model or tool identifier, tool schema, image
payload, cache key, or provider-owned remote-compaction checkpoint—Otto refuses
the provider call instead of rewriting the field. The error identifies the local
safety refusal without echoing the sensitive value. An admission-time structural
refusal happens before persistence and appends nothing. If later dynamically
produced state is refused, Otto preserves only the history already appended and
appends no provider response. Remove the sensitive value from the source or
replace it with an obvious placeholder, then retry the request.

This is defense in depth, not a guarantee that arbitrary secrets or personal
data can always be recognized. Otto deliberately avoids broad entropy guessing
and contextual guesses because source code, fixtures and ordinary prose would
produce frequent false positives. Names, street addresses, local-format phone
numbers, unsupported identity documents and unrecognized custom credentials can
still be transmitted. The guard does not OCR images or inspect decoded binary
payloads. Keep sensitive data out of prompts, workspace files intended for model
review, session files, skills and plugin output whenever possible.

## Tools and safety

Tools execute on the local machine under the capability and sandbox rules below.
Their text results still pass through the provider egress guard before they can
enter a model request.

Startup appends `~/.config/otto/AGENTS.md`, then instruction files along the
same Git-root-to-workspace directory chain as configuration. Each directory
prefers `AGENTS.md` over `CLAUDE.md`. These files supplement Otto's built-in
prompt; a project file never replaces the global file. Reads stay within
pinned directory boundaries, dynamic content is XML-escaped and redacted, and
unsafe/non-regular files or budgets above 16 KiB per file/64 KiB total fail
startup. Automatic Git status queries use the configured sandbox executor
with fsmonitor disabled; unavailable executors produce no Git status.

`SKILL.md` and `AGENT.md` must stay within their respective skill or agent
definition directory. Configured external roots and linked definition
directories remain supported.

### File tools

The six file tools are always enabled and always restricted to the initial
canonical workspace, even when `--sandbox off` is selected:

These tools and `bash` reject reserved redaction placeholders in arguments with
`redacted_input` before execution. Use original non-sensitive context, rather
than treating withheld content as a literal path, search or replacement. See
[redaction feedback](interactive-use.md#session-credential-redaction).

- `read` reads UTF-8 text with optional line offsets and limits; files larger
  than 64 MiB are rejected before being read into memory. Each call returns at
  most 4 KiB of body text, or the smaller configured output cap. A truncated
  result gives `character_offset`: use it with the same `path`, `offset` and
  `limit` to resume the selected range without losing part of a long line.
  It counts Unicode characters from zero within that selected line range.
- `grep` searches file contents with RE2-style regular expressions
  (case-insensitive matching, optional `**` glob filtering, up to 30 matches by
  default, 1000 maximum). Results also have a 4 KiB body cap; narrow the path,
  glob or pattern when truncated rather than repeating a broad query. A zero-match
  search returns `No matches.`; searches rooted in `.git` explain that it is excluded.
- `find` returns sorted regular-file paths matching `**` globs (up to 1000 by
  default, 10000 maximum).
- `ls` lists one directory level in sorted order; directories end in `/` and
  symlinks in `@`.
- `write` writes a complete file atomically and syncs the file and its
  directory to disk before it reports success. Its result includes a bounded
  text diff for new files and readable existing files up to 1 MiB. If the old
  file is binary, larger, or unreadable, the write still succeeds and the
  result explains why the diff is unavailable.
- `edit` replaces one or more unique text matches and shares the 64 MiB size
  limit with `read`. Choose one argument form: `path`, `old_text`, and
  `new_text` for a single replacement (omit `edits`), or `path` and `edits`
  for a batch (omit top-level `old_text` and `new_text`). The agent is instructed
  to combine all currently known changes to the same file into one `edits` batch
  rather than making one call per replacement. After three successful single
  replacements to the same file in one turn, Otto adds a one-time batching
  advisory to the successful result. It does not reject or repeat the edit.
  A successful multi-replacement batch resets the single-edit streak; failed
  edits do not count. Advisory tracking is bounded to 128 files per turn. Put every batch
  replacement inside `edits`; each entry has its own `old_text` and `new_text`
  and matches against the original file. An empty `new_text` deletes the match.
  When `old_text` has no exact match, `edit` retries with a
  match that ignores trailing whitespace and treats curly quotes, dashes, and
  non-breaking spaces as ASCII, and it rewrites only the part of `old_text`
  that `new_text` changes. The result shows a diff of the change. When
  `old_text` ends with a line break that a non-empty `new_text` drops, the
  edit still applies, and the result adds a note that the next line now joins
  the replaced text. Validation and match errors write nothing and are recorded
  as not started. When `old_text` matches
  more than once, the error lists the line numbers of up to 5 matches with the
  line before and after each one; when it matches nowhere, the error shows the
  file lines that best align with `old_text`, ignoring leading and trailing
  whitespace. Error listings show only current file lines and stop at 4096
  bytes.

`write` and `edit` refuse the workspace's read-only git paths listed in
[Git metadata is read-only in the Seatbelt sandbox](safety-and-tools.md#git-metadata-is-read-only-in-the-seatbelt-sandbox),
also with `--sandbox off`, and also when the path is reached through a symbolic
link or with different letter case.

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
can be outstanding. Each delay is 1 to 3600 seconds. In a chat through
`otto-connect`, the text the model writes in that turn is sent to the chat
(see [Messages and replies](servers-and-connectors.md#messages-and-replies)).

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

### `schedule`

`schedule` is for results the user asked to receive later or repeatedly, for
example "every day at 08:00 tell me whether it will rain in Beijing". `remind`
stays the model's own timer inside the current conversation. Items are kept in
`~/.otto/schedule.db`, outside any session, so they survive `/new`, `/use`,
and a restart.

| Field | Rule |
| --- | --- |
| `text` | A fixed message, 1 to 500 characters. Delivered as `⏰ <text>`. |
| `prompt` | 1 to 2000 characters. At the due time Otto runs it as the first message of a new session in the workspace where the item was created, and delivers the final reply as `⏰ <reply>`. |
| `seconds` | Delay from now, 1 to 2592000 (30 days). |
| `at` | `HH:MM` (the next occurrence) or `YYYY-MM-DD HH:MM`, in the host's local time zone, at most 30 days ahead. |
| `repeat` | `once` (default), `daily` or `weekdays`; the latter two at the `HH:MM` given in `at`. |

Exactly one of `text` and `prompt` and exactly one of `seconds` and `at` are
required. Creating a `prompt` item asks the user for approval first, whatever
its `repeat` and whatever the session approval mode: the request shows the
prompt, `at` or `seconds`, and `repeat`,
because the run uses the agent's tools (file edits and sandboxed commands)
with no user present. A denied request creates nothing. A `text` item needs no
approval. An empty or blank string counts as an absent field. Each origin (see
below) has at most 32 active items. The result names the id, the first due
time with its UTC offset, where the item will be delivered, and the current
local time:

```
scheduled s7: prompt, first run 2026-10-05 08:00 +08:00 (in 14h40m), repeats daily; delivered to this Telegram chat
now: 2026-10-04 17:20 +08:00
```

`schedule_list` lists the items of the current origin: active ones, then those
that finished, failed, or were cancelled in the last 7 days, with the error of
a failed item. `schedule_cancel {"id": "s7"}` cancels an active item; a
recurring item stops for good.

An item belongs to the origin of the turn that created it:

| Origin | Turns | Delivered by |
| --- | --- | --- |
| terminal | the TUI, the REPL, `otto --prompt`, `otto --attach` | one running TUI or REPL on this host, as a system line; the TUI also rings the terminal bell |
| web | the browser UI | an open browser UI page, as a notice |
| a chat | a Telegram or Feishu chat through `otto-connect` | that chat, through `otto-connect` (see [Messages and replies](servers-and-connectors.md#messages-and-replies)) |

Turns from an editor ACP client, scheduled runs, sub-agents, and workflows
have no origin; there the three tools return an error. If
`~/.otto/schedule.db` cannot be opened at startup, Otto prints a warning and
the tools return an error that names the file.

A prompt item runs in a process that is running when it is due: a TUI or
REPL opened on the same workspace for terminal items, `otto acp` for its
chats, or `otto serve` for every origin and workspace. No user is present
during the run, so approval requests are denied. The run is cancelled after
10 minutes. Its session stays in the session list. A run that fails before any
tool call is retried; a failure after a tool call started is delivered as
`⏰ Scheduled prompt s7 failed: <error>` and not repeated.

Delivery is at least once. A failed delivery is retried after 30 s, doubling
up to 30 minutes between attempts. An occurrence not delivered within 24 hours
of its due time is marked failed and shows in `schedule_list`. A text
delivered more than 2 minutes late ends with a line such as
`(scheduled for 2026-10-05 08:00 +08:00, delivered 2h3m late)`. A recurring
item that missed occurrences while no process could run it runs once for the
most recent one, if that is less than 24 hours old.

At the start of the next user turn from the same origin, the delivered text is
added to the conversation before the user message, as a context entry
`[scheduled s7, due ...]`, so a follow-up question can refer to it. For a chat
this is the session the chat uses at that time; for the terminal and the web
UI it is the session that created the item.

### Web tools

`web_fetch` reads one web page and returns its text; `web_search` returns a
ranked list of titles, URLs and snippets. Both run inside the Otto process, not
in the `bash` sandbox, so they work on Linux and without `bash`.

`web_fetch` takes `url` and optional `offset` and `limit` (characters). It
sends an HTTP GET with no cookies or credentials. HTML becomes Markdown with
scripts, styles and navigation removed, and links made absolute; JSON and other
text types are returned as text; images, PDFs and other binary types are an
error. The default page is 4096 characters and every call has a 4 KiB body cap
(or the smaller configured output budget), even with a larger `limit`. A page
is cut at 2 MiB of download and at the output cap; the result
header then gives the `offset` to continue from. Each call fetches the page
again. Pages that need JavaScript return only what the server sent.

`web_search` takes `query` and an optional `count` (1 to 20, default 8; Google
returns at most 10). It is always available. The backend is chosen when the
runner is built, by the first rule that applies:

1. **Google** (Custom Search JSON API) when both `GOOGLE_API_KEY` and
   `GOOGLE_SEARCH_ENGINE_ID` are set.
2. **Brave Search** (API) when `BRAVE_API_KEY` is set.
3. **ChatGPT's hosted search**, with the `chatgpt` provider and no key above:
   Otto asks the backend to run its own `web_search` tool, and no local
   `web_search` tool is offered.
4. **Bing**, with no key and any other provider: Otto reads Bing's public
   results page.

Hosted search runs at OpenAI inside the model response. It does not use Otto's
local destination checks, and Otto does not see the queries or the pages read.
`[sandbox].network = "deny"` turns it off. Whether a ChatGPT subscription
accepts the hosted tool depends on the backend; if searches fail, set a Google
or Brave key to use a local backend instead.

Local backends run without approval. A failure of the chosen backend, such as
an exhausted quota, is reported as an error; Otto does
not switch to another backend for that call. The Bing backend reads an HTML
page Bing does not document as an interface, so a layout change or Bing
blocking automated requests makes it fail with an error that says so rather
than returning "no results"; set a Brave or Google key for a stable interface.
Keys are sent only in the request to their own service and are never placed in
config or results. The result header names the backend used.

What the tools refuse:

- Only `http` and `https` URLs without embedded credentials. Loopback,
  private, link-local (including `169.254.169.254`), carrier-grade NAT,
  multicast and other non-public addresses are blocked, both for literal IPs
  and for names that resolve to them, and Otto ignores proxy environment
  variables for these requests. On a Mac where a TUN interface answers DNS with
  fake IPs, answers from 198.18.0.0/15 that the system routes through that
  interface are accepted, so the tools work; the real address is resolved
  behind the interface, and Otto cannot check that it is public.
- Everything when `[sandbox].network = "deny"`. A `/sandbox network allow`
  takes effect when the runner is rebuilt by the reload.
- URLs and queries that contain a configured secret or text shaped like a
  credential (API keys, tokens, private keys). This is a best-effort check.
- Redirects to a different host, and from `https` to `http`. The result names
  the target; the model has to request it with a new call. Redirects within the
  same host are followed, up to five.

Public web access needs no approval when `[sandbox].network = "allow"` (the
default). The same destination, credential and network checks apply to
sub-agents and scheduled runs. Legacy `[web].allow_hosts` entries are still
accepted in config but have no effect.

Web content is untrusted. Results begin with a notice saying so. A page can
still try to steer the model.

### Verification verdicts

`submit_verification` validates that a proposed verdict is consistent with real,
previously completed tool results in the current session. It does not run a
test, lint, build or other check. Evidence can come from any tool, identified by
its prior `tool_call_id`, so the capability does not privilege command names or
parse command output.

A submission contains a bounded claim, one of `passed`, `failed`, `blocked`,
`not_run` or `inconclusive`, evidence references, and optional limitations.
Each evidence reference classifies its relationship to the claim as `supports`,
`contradicts`, `blocker` or `context`. Otto checks that every referenced result
is durably present, has typed terminal operation metadata, is not duplicated,
and is structurally compatible with the verdict. For example, `passed` needs
successful completed supporting evidence and cannot include contradictory or
blocking evidence; `not_run` cannot cite evidence. Rejected submissions return
an actionable tool error rather than recording an accepted verdict.

This is an evidence-integrity check, not a semantic proof. Otto does not decide
whether a check adequately covers the claim, interpret a tool's text, infer
whether an error is a product or infrastructure failure, impose a completion
gate, or guarantee that later changes leave an accepted verdict current. The
agent must submit only after the relevant evidence-producing calls, describe
coverage limits, and report failed, blocked, not-run and inconclusive work
without calling it passed.

### `list_models`

For `claude`, `openai-compatible`, and `chatgpt`, `list_models` discovers the
active provider connection's model catalog, with metadata only where actually
available. Claude supplies its catalog through `GET {base_url}/v1/models`; an
OpenAI-compatible endpoint uses `GET {base_url}/models`; ChatGPT discovery uses
the signed-in connection. Otto performs discovery with the existing provider
authentication: the provider's API-key environment variable (and optional Claude
workspace binding) or credentials from `otto login`. Credentials stay in the
runtime, not in tool arguments or `bash`. Child agents do not get this tool.

Catalog entries do not guarantee that every model is usable for every task.
Providers may omit pricing, capabilities or other metadata; Otto has no built-in
price ranking. Returned metadata is untrusted data, not instructions. The parent
must distinguish known metadata from its own suitability judgment and must not
invent IDs or prices. Discovery is on demand, not a full catalog attached to
every request: the parent calls once before ordinary delegation and reuses the
result within the current connection, refreshing when that connection changes.
Explicit user choices, named-agent defaults and the optional deprecated `subagent_models`
override do not require catalog selection.

ChatGPT catalog requests use a separately maintained Codex catalog compatibility
version, not Otto's release version. Successful nonempty results are saved to
`~/.otto/cache/chatgpt-models.json` for three days, so later sessions can reuse
them without a request. The cache is scoped to the endpoint, signed-in account
and compatibility version; a change to any of these requires a fresh request.
An empty result or request error is not saved. A ChatGPT model request rejected
with HTTP 400 or 404 invalidates the cache for the next discovery; it does not
automatically rerun that request or a child task. Configured model tables are
defaults, not a catalog allowlist.

If discovery fails, returns an empty catalog, or does not establish suitability,
the parent uses the session model fallback without blind discovery retries. To
recover discovery, check the selected provider and endpoint, export its configured
API-key variable or renew ChatGPT authentication with `otto login`, then start a
new connection. A provider can reject a catalog ID; inspect partial work and
possible side effects before deciding whether retrying the child is safe.

### Workflow tools

Parent conversations can discover, inspect, start and check existing durable
workflows with `workflow_list`, `workflow_inspect`, `workflow_start` and
`workflow_status`. These are distinct from interactive sub-agent delegation
and from scheduling a future prompt. See [conversational workflow selection
and setup](automation.md#choosing-workflows-from-conversation) for the tool
roles, examples, human gates and recovery limitations. Child agents and
workflow steps do not receive the parent workflow controls.

### Sub-agents

The parent normally uses [`list_models`](#list_models) to choose an adequate
model, putting its short reason in the task description. Explicit user choices
and named-agent defaults take precedence; unavailable/uncertain discovery falls
back to the session model, not an invented ID. No manual list is required.
Without an explicit call model, precedence is named-agent default, optional
deprecated `subagent_models` first entry (legacy profiles only), then session
model. Changing the provider connection disables that list; select the intended
model or remove conflicting overrides and restart. Check rejected model IDs and
partial work before retrying: failed child work is not automatically repeated.

`agent_status.model_source` is `explicit`, `definition`, `configured_first`,
`parent`, or `unknown` for older records. It describes selection provenance,
not price or suitability. Persisted tasks keep their model/source across restart.

`agent` starts/resumes a child and returns its task ID immediately. Settle shared
interfaces and assign each writable file one owner before parallel work. Keep
coupled changes together; coordinate contract changes before dependent edits.
The parent integrates and verifies results. Ownership instructions are not
filesystem locks or grants; tool allowlists/write policies remain authoritative.

A child starts with `context: "fresh"` (only its prompt) unless the call or
its named agent definition sets `context: "inherit"`. With `inherit`, the
child's transcript starts with a copy of the parent conversation up to the
`agent` call. To start from an earlier point, the parent sets `fork_before`
to the text of an earlier user prompt that started a turn, either the full
text or its first line. The child then receives the parent conversation as
it was just before that prompt's turn, the same history `/fork` would keep at
that point, read from the parent's session file. Turns that compaction has
since summarized are included in their original form. `fork_before` implies
`inherit`; it is rejected with `context: "fresh"`, with `resume`, and in
sessions without a saved file (`--no-session`). Exactly one prompt must
match; otherwise the error lists the prompts that can be used (first line,
newest first, at most 20). Steering text and prompts that opened wake turns
are not fork points, and a prompt whose text appears more than once cannot
be selected. The parent session is only read.

### Child recovery after process loss

Every activation of a saved parent inventories only that parent's direct child
transcripts, even if its own last turn finished normally. `agent_status` shows
reconstructed task snapshots; unfinished lost-owner children are **interrupted,
not running**. This restores identity and inspectability, not suspended futures,
processes, approval objects or shell state. No recovered child starts automatically.
In-memory (`--no-session`) tasks have no cross-process recovery.

Discovery validates parent linkage, task/header/filename identity and conflicting
IDs, rejects symlinks, and bounds directory entries (4096), each transcript
(16 MiB) and aggregate child reads (64 MiB). Incomplete/corrupt/unavailable scans
produce warnings and status diagnostics rather than silently claiming no tasks.
A durable admission without a usable child spec is visible but cannot resume.
Diagnostics remain available at activation even when provider execution is unavailable.

Before asking `agent` to `resume` an interrupted task, read its transcript and
check any uncertain tool effects. Resume preserves the logical task ID and child
transcript, atomically admits one new attempt, and records additive `resumed`
lineage; it does not infer that a newly constructed replacement is the original.
Concurrent resume attempts cannot both start. A failed repair or incomplete scan
makes resume unavailable; correct the reported storage/linkage problem and reopen
instead of reconstructing history or blindly rerunning commands. Completed and
external ACP tasks cannot resume. Recovered snapshots do not claim historical
usage/timestamps that are absent from lifecycle facts; resumed progress counters
start fresh for that attempt.

Terminal lifecycle facts precede completion notification. Stable task/attempt
input IDs let reopening regenerate pending notices without duplicate delivery.
Persistence failures stay visible; they never count as a successfully recorded
completion. Independent custom events, including local memory receipts, do not
turn a completed attempt back into an unfinished one. Upgrade using the normal
source-install procedure and restart all processes before reopening saved sessions.

Assign bounded file scopes and request findings with file/line evidence. After
delegation, the parent relies on the child report and reads its saved report
sections rather than repeating source inspection. Follow-up questions can be
sent only while the child is queued or running; `resume` only accepts interrupted
tasks, not completed ones. Continue independent work
after delegation, or finish the current response. A
child report or completion is queued in the parent mailbox; if the parent is
idle, the runtime starts a wake turn and delivers it as a `[task-notification]`.

Research downloads, search output and local analysis stay in the child's
context. The child is instructed to return only verified conclusions, essential
original-source references, uncertainty and decisions needed from the parent.
Parent-facing previews remain bounded to 4 KiB (or the smaller configured
budget), plus truncation and report-file metadata. For longer research, a child
with permitted `write` access saves `report.md` with Markdown section headings
in its assigned `.otto/reports/<session-id>/<task-id>-<unique-id>/` directory.
The per-session directory separates names between sessions only; it is not
access control. Its final
handoff contains a concise summary, uncertainty, the report path and a section
index with line numbers. This does not grant additional write permissions or
expand an assigned file scope.

If final text exceeds the parent preview budget, the runtime automatically
saves the complete redacted answer as `final.md` and generates `index.md` with
Markdown heading line numbers. These two names are reserved for runtime use.
The completion notification and `agent_status` include the full report path,
byte/line counts and index path outside the preview cap. Use `read` with the
report path and the index's 1-based `offset` plus `limit` to retrieve a section;
when a selected range is still truncated, use the returned `character_offset`
with the same line range. Reading a saved report never restarts the child.
Reports without Markdown headings remain readable using the same cursor.

Each task attempt gets a unique directory, including resumed tasks and sessions
that reuse task IDs. Reports persist across child completion, cancellation and
process restart; scratch cleanup does not remove them. They are workspace-local,
ignored by Git and remain until explicitly removed. Existing read permissions
still apply. File storage failures produce an explicit warning instead of a
nonexistent report pointer and do not change the task outcome. If only index
creation fails, the complete report path is still returned. Runtimes without a
bound workspace cannot save report files. Completed tasks from before this
feature are not retroactively exported.

Complete child transcripts and task results remain recorded as before.
`agent_status` defaults to bounded status/result text without recent steps;
`include_history: true` includes recent steps within the same preview cap.
`agent_report` messages remain bounded handoffs while a child is running; use
an authored report file for longer interim content. Automatic file capture
applies to final answers.

A native child with `bash` receives a private workspace-relative directory under
`.otto/tool-output/<session-id>/research-<id>/`. Download each pinned upstream source once
there, then analyze it with scripts instead of fetching repeated large snippets.
This directory is deleted when the child task exits, including error, queued
cancellation and normal cancellation. Cleanup operates through pinned directory
handles and never follows links outside the owned tree or deletes siblings.
Keep durable deliverables outside it and cite original URLs/file locations, not
temporary paths. This scratch directory is separate from the durable transcript.
Filesystem failures or abrupt process termination can prevent cleanup. Without
`bash`, no scratch directory is allocated; existing web/file tools still work.

External coding agents can be selected with `agent` names `acp:codex`,
`acp:claude`, and `acp:pi`; install their `codex-acp`, `claude-agent-acp`, or
`pi-acp` executable on the absolute directories in `PATH` first. Otto starts an
ACP session in the current workspace and returns its final text through the
normal task notification. They share the sub-agent concurrency limit.

External agents use their own authentication, tools, sandbox and permission
settings. They do not inherit Otto's sandbox, child tool allowlists, or write
policies. ACP permission requests with a reviewable action and an `allow_once`
option go through Otto's approval service; other requests are declined. This
cannot prevent an external agent from performing actions it does not ask ACP
to authorize. Review the external agent's own settings before use.

External agents accept fresh context and their own model settings. `agent_send`,
`agent` resume, inherited context and per-call model selection are unsupported
for them. Cancellation sends ACP `session/cancel` and, after a bounded grace,
terminates the adapter process group. External adapter availability and
capabilities depend on the installed version. Otto passes a restricted environment:
home and runtime paths, locale, proxy and CA settings, agent config directories,
provider base URLs, and variables ending in `_API_KEY` or `_AUTH_TOKEN`. Other
environment variables are not passed to the adapter.

When only child dependencies remain, call `agent_yield` with `{}`. It returns
immediately and requests ending the current model turn at a safe boundary; it
does not wait inside the tool, cancel a child, or poll. All tool results in the
batch are persisted before yielding. Tool errors and approval replanning take
precedence, and queued notifications or user guidance are processed instead of
being stranded at the yield boundary. Later child notifications or user input
resume the parent through the existing runtime wake path. Calling it with no
queued/running child, unsupported arguments or cancellation does not yield.
Only the parent receives this tool; it is not the old blocking `agent_wait`.

`agent_status` detail includes live activity: `tool`, `provider_or_runtime`,
`waiting_for_approval`, or `approval_review` (and `unknown` if approval state
cannot be read). Provider/runtime covers model calls, retries and orchestration;
it does not claim an in-flight request is stalled. The last-tool field remains
historical. Do not cancel children merely because that field or counters have
not changed; use actual failure/timeout evidence or a changed task requirement.

Use `agent_status` to inspect progress or retrieve a report path, `agent_send`
to send guidance, and
`agent_cancel` to stop a task, and `agent_yield` when only dependencies remain.
Do not poll status to simulate waiting. Children
never receive these tools, so a child cannot start or stop another child.

`agent_cancel` takes a task id or name and cancels that task. Cancellation is
cooperative: a queued task is released before it starts, and a running one
stops at its next checkpoint; a task inside a tool call stops once that call
settles, so a call that already changed something is never abandoned
half-done. The task ends as `canceled` and its `[task-notification]` arrives
the usual way. `/task cancel <id|name>`,
`POST /v1/sessions/{id}/tasks/{task_id}/cancel`, and ACP's task cancel reach
the same operation. A finished task cannot be canceled and answers with an
error instead.

#### Stopping a turn that makes no progress

A turn whose model keeps issuing the same tool call, or keeps receiving tool
errors, would otherwise run until a deadline. Interactive turns have no
deadline unless you configure one; chat and `--prompt` turns stop after
[`[agent].unattended_turn_timeout`](getting-started.md#configuration)
(default `30m`). Otto watches this itself and ends such a turn with
the error `no progress: the same <tool> call repeated <n> times in a row`,
`no progress: <n> tool calls in a row failed (last: <tool>)`, or
`no progress: a tool-call cycle repeated <n> times without progress (last: <tool>)`.
This applies to every turn, including each sub-agent's, which inherits your
limits. A turn that ends this way is not a persistence failure: the session
continues and you can send the next prompt.

Two calls count as the same call when the tool name and the arguments are the
same; key order and spacing in the arguments are ignored. Otto also detects
repeated cycles of 2–4 calls when at least one call fails and all calls and
result text repeat unchanged. For example, an edit rejected before writing,
followed by a read of the same unchanged file, cannot loop indefinitely.
Cycle tracking resets after any operation that may have changed state; only
declared read-only tools and operations known not to have started participate.
When extension hooks are configured, only consecutive checks run because hook
effects are not covered by tool safety.
Longer cycles and successful-only alternating cycles are not detected. Use
`agent_status` and `agent_cancel` for these; chat and `--prompt` turns also
have the unattended deadline.

`[agent].no_progress_repeats` (default 8) and
`[agent].no_progress_failures` (default 12) set the limits. The repetition
limit applies to both identical consecutive calls and unchanged failed cycles.
Set one to `0` to disable that condition; set both to `0` to disable the guard.
Raise them if a deliberate poll loop, such as watching a long build, is being
stopped early. Values must be between 0 and 1000.

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

The GitHub CLI recipe adds its configuration directory to `read_paths`, which is
an exception to the built-in read denylist entry `~/.config/gh`. The wizard uses
an existing absolute `GH_CONFIG_DIR`, or defaults to `~/.config/gh`. The recipe
does not add `GH_CONFIG_DIR` to `allow_env` and prints no launch command,
because `gh` finds `~/.config/gh` through the real `HOME`. The exception can
expose saved GitHub credentials to shell commands. Run `gh auth login` outside
Otto first if the configuration directory is missing.

Choose `check` to test sandbox startup and, when selected, GitHub CLI availability
(`gh --version`) and whether the configuration directory is readable. The check
does not contact GitHub or verify authentication or network connectivity.
Choose `save` to write the reviewed configuration.

### `bash` sandbox policy

On macOS, the default is `--sandbox auto`, which means Seatbelt. The sandboxed
command can write the whole workspace, the admitted linked worktrees and the
Otto-managed private `tmp` and `cache` directories beneath your user cache. It
can read every path on the host except the paths on the
[read denylist](#a-sandboxed-command-reads-every-path-except-the-read-denylist).
`HOME` is your real home directory, so `~` in a command names your home
directory. The private per-session `home` directory does not exist. A write
under `~` outside the writable set fails with a write denial, and `TMPDIR` is
the writable scratch location.
Otto keeps generated sandbox profile files in a separate private `profiles` directory
that the sandboxed child cannot read. Source, tests and generated files remain
writable. Git program settings/hooks and workspace identity are protected;
see [Git metadata restrictions](#git-metadata-is-read-only-in-the-seatbelt-sandbox).

When the selected workspace is a Git worktree, Otto also admits the other
worktrees of that repository. Candidates come from `git worktree list
--porcelain` run in the workspace. A candidate is admitted only when its own
`.git` entry links back to the workspace's common Git directory: a regular
`.git` file whose `gitdir:` target is a directory directly below
`<common>/worktrees`, or a `.git` directory that is `<common>` itself. The
listing alone is not sufficient, because it is derived from
`.git/worktrees/<id>/gitdir` files that a sandboxed command can write.
Records marked `bare` or `prunable`, `.git` entries that are symbolic links,
and paths that fail to canonicalize are not admitted. Bash can read and write
admitted worktrees without a `read_paths` entry; their `.git`
metadata and Otto workspace identity stay read-only. This does not admit a
similarly named directory, a different repository, or change the file tools'
selected-workspace boundary. Otto rediscovers this set at startup and
`/sandbox reload`.

`network = "allow"` is the default and permits ordinary IP networking and local
IP binds. `network = "deny"` blocks IP networking and local binds. There is
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
`network = "deny"`, cache replacement, `deny_read_paths` and `read_paths` no longer
constrain the shell.

Under Seatbelt, the model may set `sandbox_permissions` to
`require_escalated` with a justification. Otto prepares the exact command and
routes it through the session's approval policy before dispatch. AUTO may
authorize the exact operation after isolated review; MANUAL and human-required
requests wait for your decision. Approve those through the TUI, Web,
chat card, or `/approve <request-id> [feedback]` in the REPL; use
`/deny <request-id> [feedback]` or `/withdraw <request-id>` to decline or withdraw.
The approved command runs once through the unconfined driver with the same
filtered environment. It does not require the model to issue the command again.
Subagent requests use the same service and are scoped to their task. A proposal
confirmation cannot grant elevation. Execution requests expire after ten
minutes; cancellation withdraws pending foreground requests. Persistent
read-path requests explicitly disclose that they change sandbox configuration.

Commands matching `[sandbox].excluded_commands` run through the same unconfined
driver without an escalation approval. An excluded command in one of the
[classified commands](#classified-commands-wait-for-approval-inside-the-sandbox)
classes still waits for that approval and then runs unconfined. Exclusions
apply to child agents and `otto --prompt` runs as well, and only to simple
commands; see [`[sandbox]`](getting-started.md#configuration) for the entry
syntax and the rule.

### A sandboxed command reads every path except the read denylist

Seatbelt allows reads of all paths and then denies the paths below. Every path
that is not on this list is readable, including files in your home directory
that are not named here. A denylist protects listed locations only: a
credential stored at an unlisted path is readable and, with
`network = "allow"`, can be sent off the host.

The built-in entries are relative to your home directory. Each entry denies the
path and everything below it, whether or not the path exists:

- Credentials of tools and services: `.ssh`, `.gnupg`, `.aws`, `.azure`,
  `.config/gcloud`, `.kube`, `.docker`, `.netrc`, `.git-credentials`, `.npmrc`,
  `.pypirc`, `.cargo/credentials.toml`, `.cargo/credentials`,
  `.gem/credentials`, `.m2/settings.xml`, `.terraform.d/credentials.tfrc.json`,
  `.config/gh`.
- Credentials of coding agents, Otto's own included: `.otto/auth`,
  `.claude/.credentials.json`, `.codex/auth.json`.
- Shell history: `.zsh_history`, `.bash_history`, `.zsh_sessions`.
- Personal data kept by macOS and browsers: `Library/Keychains`,
  `Library/Cookies`, `Library/Safari`,
  `Library/Application Support/Google/Chrome`,
  `Library/Application Support/Firefox`, `Library/Messages`, `Library/Mail`.
- Every private sandbox state root `~/Library/Caches/otto-sandbox-*`, except the
  `tmp` and `cache` directories of the running session.

Three configuration and runtime inputs change the effective list:

- `[sandbox] deny_read_paths` adds entries. Each entry is an absolute path or
  starts with `~/`, like `read_paths`, and need not exist. It takes effect on
  `/sandbox reload`. An entry that equals or contains the workspace or a linked
  worktree is rejected and the sandbox does not start. An entry below the
  workspace is accepted: reads of that subtree are denied and writes stay
  allowed.
- A built-in entry that equals or contains the workspace or a linked worktree
  is skipped, because the sandbox cannot work without reading it. `/sandbox`
  prints `Sandbox deny_read_paths <path>: contains a writable root; not applied`
  for each skipped entry.
- `[sandbox] read_paths` lists exceptions inside the denylist. An entry is
  active only when its resolved path equals or lies below a denied path. An
  entry that is not below any denied path, or that is an ancestor of a denied
  path, is inert: it changes nothing, and `/sandbox` prints
  `Sandbox read_paths <path>: not below any denied path; remove it from read_paths`.
  For example, `~/.otto` does not lift the denial of `~/.otto/auth`. Otto
  rejects `read_paths` that overlap Otto's private sandbox state. An
  unavailable configured `read_paths` entry is skipped until it exists again;
  remove obsolete entries from the configuration to keep the policy clear.
  Existing configurations load unchanged.

Add only the narrow exceptions you need. An exception for a credential
location lets command code read it and, with `network = "allow"`, send it off
the host.

The `bash` tool's `sandbox_read_path` argument and `/sandbox allow <path>`
accept only a path that the denylist covers. For any other path the tool
returns `path is already readable; run the command without sandbox_read_path`
and the REPL reports `path is already readable; no read_paths entry is needed`.

Writes are unchanged. Limitations of this policy:

- Commands that match `excluded_commands` run unconfined and read every path,
  including `deny_read_paths` entries.
- `HOME` is the real home directory, but the cache variables
  `XDG_CACHE_HOME`, `GOCACHE`, `GOMODCACHE`, `NPM_CONFIG_CACHE`, `PIP_CACHE_DIR`,
  `UV_CACHE_DIR` and `CARGO_HOME` point into the private `cache` directory.
  The cargo, go, npm, pip and uv caches therefore download dependencies again in
  every session, and cargo does not read `~/.cargo/config.toml` (registry
  mirrors, build settings) or `~/.cargo/credentials.toml`. `RUSTUP_HOME` is not
  set, so the rustup proxy finds `~/.rustup` through `HOME`.
- `ZDOTDIR` points to the private `tmp` directory, so `zsh -c` does not source
  `~/.zshenv`, `~/.zprofile` or `~/.zshrc`. `BASH_ENV` and `ENV` stay withheld.

### Classified commands wait for approval inside the sandbox

Under Seatbelt, Otto parses each `use_default` command and each command
matching `[sandbox].excluded_commands`, and classifies it before it runs. A
command in one of two classes creates an approval request; after approval it
runs inside the sandbox, without escalation, or through the unconfined driver
when it is an excluded command. The model does not need to give a
justification.

| Class | Commands | Authority |
| --- | --- | --- |
| Changes external state | `git push` (not `--dry-run`); `gh` except read-only subcommands such as `pr view`, `issue list`, `run watch`, `search` and `api` with `GET`; `curl` with data, form, upload or a method other than `GET`/`HEAD`; `wget` with post/body data or such a method; `npm`/`pnpm`/`yarn` `publish`, `unpublish`, `deprecate`; `cargo publish`, `yank`, `owner`; `docker`/`podman push`; `twine upload`; `gem push`; `kubectl` apply, create, delete, patch and other mutating verbs; `helm` install, upgrade, uninstall, rollback, push; `terraform`/`tofu` apply, destroy, import | `Boundary` |
| Deletes workspace or repository content | `git reset --hard`; `git clean -f` (not `-n`); `git checkout -f`, `--` or `.`; `git restore` without `--staged` only; `git switch -f`/`--discard-changes`; `git stash drop`/`clear`; `git branch -D`; `git reflog expire`/`delete`; `git filter-branch`/`filter-repo`; `rm -r`; `find -delete` | `Bounded` |

Every simple command in a list or pipeline is checked, including commands in
substitutions, commands after `env`, `sudo`, `timeout`, `xargs` and similar
wrappers, and commands run by `find -exec`. Arguments written as `$VAR` or
other expansions are not known; flags written literally after them still
count, so `find "$DIR" -delete` is classified.

| Approval mode | Changes external state | Deletes workspace or repository content |
| --- | --- | --- |
| allow rule matches | runs | runs |
| AUTO | isolated review | runs, recorded as bounded authority |
| MANUAL | you decide | you decide |
| `never` policy | rejected | runs, recorded as bounded authority |
| YOLO | runs | runs |

An `allow` command-prefix rule such as `["git", "push"]` approves matching
requests in every mode. A `git push` review requires branch, remote and
outgoing-diff facts; without them the request goes to you.

Limits of this check:

- It reads the command text. Programs started by a script, `make`,
  `npm run`, `python -c`, `bash -c` or `eval` are not classified. Only
  `[sandbox] network = "deny"` stops network writes from them.
- Cloud CLIs such as `aws`, `gcloud` and `az` are not classified.
- A `require_escalated` request that is not an excluded command keeps its
  own approval.
- `/approve <id> always` adds the command to `excluded_commands`; a
  classified command added this way still asks on each run.
- With `--sandbox off`, on Linux and in `otto --prompt` runs, which have no
  approval flow, commands are not classified.

### Git metadata is read-only in the Seatbelt sandbox

Git runs hooks and the programs named in its configuration outside the
sandbox, so a sandboxed command must not write them. Under Seatbelt, these
paths are read-only in the workspace and in every admitted worktree, whether
or not they exist, and no setting turns this off:

- `<workspace>/.git` (the entry itself, so it cannot be renamed or removed)
- `<workspace>/.git/config`
- `<workspace>/.git/config.worktree`
- `<workspace>/.git/commondir`
- `<workspace>/.git/hooks` and everything below it
- below `<workspace>/.git/modules` (submodule Git directories), at any depth:
  files named `config`, `config.worktree` and `commondir`, and every `hooks`
  directory with its contents
- below `<workspace>/.git/worktrees`, at any depth: files named
  `config.worktree`

`.git/worktrees/<id>/gitdir` and `.git/worktrees/<id>/commondir` stay
writable because `git worktree add` creates them. The worktree admission
described above does not trust them. A sandboxed command can still point a
linked worktree's `commondir` at another directory, and the user's next git
command in that worktree reads its configuration from there. Nested
repositories (`<dir>/.git` below the workspace) are not covered, so `git
clone` and `git init` into a subdirectory work in the sandbox.

The `write` and `edit` tools run outside the sandbox and refuse the paths
above, with or without `--sandbox off`. They refuse more than the sandbox
does, because they never need these paths: at any depth in the workspace, a
`.git` entry, and below any `.git` component the files `config`,
`config.worktree`, `commondir` and `gitdir` and every `hooks` directory. The
comparison ignores letter case the way APFS does (including U+212A KELVIN
SIGN for `k` and U+017F LATIN SMALL LETTER LONG S for `s`) and applies after
symbolic links are resolved, both before the write starts and again at the
final rename.

Commands that fail in the sandbox because they write these paths: `git init`
in the workspace, `git config` without `--global`, `git remote add` and
`set-url`, `git push -u`, `git branch --set-upstream-to`, creating a branch
that tracks a remote branch, `git submodule init`, `git submodule update`
when it clones a submodule into `.git/modules`, and hook installers such as
`pre-commit install`.

Commands that are not affected: `git commit`, `checkout`, `branch` without
tracking, `fetch`, `pull`, `merge`, `rebase`, `stash`, `worktree add`,
`clone` into a subdirectory.

To run one of the failing commands, the model sets `sandbox_permissions` to
`require_escalated`, and you enter `/approve <id>`.

### A failed command lists what the sandbox denied

When a `bash` command exits non-zero or is killed by a signal, its result can
include a `sandbox_denied:` section between the stderr block and the
`exit_code:` line. Each line is one refused operation and its target, for
example `file-read-data /Users/me/.ssh/config` or
`file-write-create /path/to/workspace/.git/hooks/pre-commit`. The system
prompt instructs the model to answer a denied read outside the workspace by
requesting a persistent read grant with Bash's `sandbox_read_path` argument, and a denied write to `.git` metadata or
outside the workspace by requesting `require_escalated`.

When a listed denial has a `file-read` operation, the section ends with the line
`hint: a denied read is on the sandbox read denylist; request a persistent exception with sandbox_read_path, or an elevated run with sandbox_permissions`.

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

The default shell timeout is 120 seconds. The general tool-output cap is
50 KiB; `read`, `grep` and `web_fetch` additionally bound body text to 4 KiB. Bash previews each stdout/stderr stream at at most 4 KiB,
or the smaller `agent.max_output_bytes` setting. Short output is unchanged;
long output retains the head and tail, reports the normalized/redacted byte
count and omitted bytes, and gives a workspace-relative `full_output` path.
Exit code, signal, timeout/cancellation and sandbox denials remain visible.

Overflow logs are streamed after UTF-8 normalization and secret redaction into
exclusive `0600` files under `.otto/tool-output/<session-id>/`, with a local
`.gitignore` in `.otto/tool-output/`. The per-session directory separates names
between sessions only; it is not access control.
The model is instructed to remove specific temporary logs as soon as analysis
is finished and no task needs them. As a fallback, the runtime automatically deletes its own
logs once the current turn and all outstanding child tasks have ended, including
failure and cancellation. Queued children also retain a lease, so ending the
parent turn does not remove logs they may still need. Cleanup uses pinned
directory handles and only removes files generated by that runtime; it never
recursively deletes the shared directory or another session's logs. The empty
directory and its `.gitignore` may remain. Historical paths are temporary, not
durable attachments: keep relevant findings in the response. Filesystem cleanup
failures are retried on the next idle transition and runtime drop; abrupt process
termination can leave orphan logs. There is no automatic disk quota. If storage
fails, execution continues and the result explicitly says that the full log is
unavailable; partial files must not be treated as complete results. Commands
that produce only short output create no log.

For verbose jobs, the agent is instructed to prefer quiet flags or write a small
script that captures and analyzes logs locally, removes its own temporary logs
on both success and failure, then returns only a checked summary and relevant
failure excerpts. The script must preserve the original
command's exit status, account for pipeline failures, and verify expected tests
or stages completed. No matches for `error` does not establish success; missing
summaries, timeouts and parsing uncertainty must be reported. Saved logs are
untrusted data. Analyze them through sandboxed `bash`, or use `read` with
`offset` and `limit` for a relevant excerpt instead of returning the whole log.
The usual `read` file limits still apply; use a streaming script for logs larger
than 64 MiB or containing binary data.
Override them with `--shell-timeout` and `--max-output-bytes` or `[agent]`.

---

Previous: [Interactive use](interactive-use.md) · [Documentation index](../README.md) · Next: [Servers and connectors](servers-and-connectors.md)

## Provider endpoint trust

Provider origin authorization is separate from endpoint syntax and API keys.
Custom Claude/OpenAI-compatible endpoints require independent user/managed
origin grants before credential lookup or network access. Local/private
origins require the same explicit grant; plaintext HTTP needs an additional
acknowledgement. Literal metadata/link-local targets are rejected and provider
redirects are disabled, including when callers try to inject HTTP clients.
See [endpoint trust and recovery](getting-started.md#endpoint-trust).

This does not detect every malicious HTTPS service or pin DNS. Grants authorize
the entire origin, including every path. User/managed config is trusted;
approved shell access that can modify it remains a security-sensitive authority.
Project configuration, model changes and URL overrides cannot grant trust.

Provider transports check every DNS answer before connecting. A result set with
any metadata/link-local address is rejected in full, including mapped IPv4 and
well-known NAT64 forms; DNS cannot silently fall back to another resolver.
Private/local addresses remain usable only behind the independent origin grant.
Owned provider completion and catalog clients disable environment/system proxies
(including `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`) because a proxy could
resolve the destination outside this policy. Transparent OS/network routing is
outside Otto's resolver boundary; grants are still not service certification.
