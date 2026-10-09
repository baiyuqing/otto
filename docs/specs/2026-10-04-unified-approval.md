# Unified approval service

Session policy extension: [session automatic approval](2026-10-05-session-auto-mode.md)
supersedes this design's assumption that every decision requires a human round trip.

Status: approved design; implementation and acceptance verification in progress.
Source baseline: `59dfe6e`, the checkout supplied for this discussion.

## Goal

Every operation requiring human approval creates a structured request through
one native service. TUI, Web, Telegram, Feishu, CLI and ACP clients present and
decide the same request. Models know the approval capability and decide when
to invoke it for conversational consent or to submit a prepared action for
review. They can discuss a pending request, but cannot grant authority.
Correct lifecycle, recovery and
frontend coverage are acceptance requirements, not optional later additions.

Approval is an in-process application component with a persistent journal,
not a separately deployed server. A running `otto serve` owns it for attached
clients. Standalone Otto runtimes compose the same service. Multiple processes
using the same Otto state directory coordinate through database transactions
and fenced application claims, not process-local channels alone.

## Current source inventory

| Producer / entry point | Baseline behavior | Required integration |
| --- | --- | --- |
| [Bash](../../crates/otto/src/tool/bash.rs), [app approval loop](../../crates/otto/src/app/approval.rs) | Process-local newest request per session; tool errors encode requests as text; approval grants an exact-command retry | Structured authorization before dispatch; no text parsing or model-generated retry |
| [Sandbox changes](../../crates/otto/src/app/sandbox.rs), [controller grants](../../crates/otto/src/app/mod.rs) | Read grants and command exclusions amend config and reload; frontend confirmation varies | Versioned preview and explicit persistent scope, with config/reload reconciliation |
| [Memory tools](../../crates/otto/src/tool/memory.rs), [service](../../crates/otto/src/memory/service.rs) | Model writes/forgets queue durable candidates; human review commits | Publish every candidate; route all review decisions through approval and preserve memory validation |
| [Reflection](../../crates/otto/src/reflection/mod.rs) | Memories become candidates; vetted skills are written without individual human approval | Reuse memory requests; require a separate skill-change approval after vetting |
| [Workflow](../../crates/otto/src/approval/workflow.rs) | Durable approval steps have independent requests, decisions and workflow events | Central decision authority; workflow transactions apply decisions and advance dependent steps |
| [Profile changes](../../crates/otto/src/server/config.rs) | Server-local expiring previews and confirm tokens | Common authorization bound to a typed preview and base config version |
| [Approval dialogue](../../crates/otto/src/app/approval_control.rs), [model controls](../../crates/otto/src/tool/otto.rs) | Restricted help/query/withdraw/queue dialogue for Bash requests | Generalize to exact request IDs; add model-initiated confirmation |
| [Subagents](../../crates/otto/src/subagent) | Child operations have separate task/session lifecycles | Requests identify originating task and parent audience; parent cannot approve |
| [MCP](../../crates/otto/src/mcp), [MCP tool adapter](../../crates/otto/src/tool/mcp.rs) | External tool dispatch has no common human-interaction service | Enforce configured authorization before dispatch; route supported server elicitation separately |
| [ACP permission](../../crates/otto/src/acp/approval.rs), [attach](../../crates/otto/src/acp/attach.rs) | Bash-oriented standard permission mapping | Typed mapping, capability negotiation, durable pending queries and cross-client decisions |
| [Chat bridge](../../chat-connector/internal/bridge/bridge.go), [memory cards](../../chat-connector/internal/bridge/memory_cards.go) | Shared card transport, separate permission waiter and memory-card routing | One request presenter and callback path, with durable recovery |
| [Core agent loop](../../crates/otto-core/src/agent/mod.rs), [wire events](../../crates/otto-core/src/wire/events.rs) | Sequential tool dispatch; requests discovered after a completed agent step | Explicit approval wait/settlement and a batch execution barrier |

Direct human commands already specifying a complete action can provide its
consent, rather than requiring a second prompt. Such an action still passes
through the same validated decision/application path and records the actual
human command as its source. A model invocation cannot use that shortcut.
Read-only calls within existing authority do not acquire new prompts. File
workspace checks and platform sandbox restrictions remain mandatory even when
an unrelated plan has been approved.

## Ownership and dependencies

`otto-core::approval` owns transport-neutral types, validation and typed wait
events. Core agent/tool contracts describe waiting and batch termination;
they do not open databases, select UI clients or implement native policies.

`otto::approval` owns request creation, immutable subjects, persisted decisions,
visibility, event cursors, expiry, withdrawal, application claims and recovery.
It is injected at the native composition root. No frontend owns pending state
or implements the approval policy independently.

Domain adapters own subject preparation, current-version validation, applying
the authorized operation and reading durable application receipts. These are
real consumer boundaries: sandbox, memory, workflow, config, reflection and
external tools have different atomicity and recovery contracts. They are
explicitly wired; model names and arbitrary JSON never select an executable
handler. Approval holds neither a shell executor nor business SQL itself.

Frontends own rendering and transport-local request correlation only. HTTP,
ACP, chat card IDs and native widgets adapt into the same service. Existing
domain request states become projections/application state, never competing
sources of human decisions.

## Request and decision contract

Two semantic kinds share the lifecycle:

- `authorization`: permission to perform an exact operation or establish a
  precisely described grant. Only a domain adapter may prepare this kind;
  a model may request review of a valid prepared subject reference.
- `confirmation`: human acceptance/rejection of a proposed plan or approach.
  Models may create it. It grants no filesystem, process or external authority.

General questions/choices are elicitation, not authorization. They can reuse
presentation infrastructure, but cannot produce an authorization receipt.

| Request field | Contract |
| --- | --- |
| `schema_version`, `id` | Versioned wire contract; globally unique opaque service-generated ID |
| `kind`, `source`, `subject` | Typed producer and immutable subject; no callback code or arbitrary handler identifier |
| `scope` | Authority owner, workspace/store identity and audience; optional session/turn/task/run correlations |
| `revision`, `subject_digest` | Bind the exact reviewed version; digest includes action, target/base revision, authorization options and scope |
| `title`, `reason`, `details` | Sanitized review content or a versioned domain reference resolved on authorized detail reads |
| `options` | Opaque IDs, human labels and typed effects; generated/validated by service and adapter |
| `wait_policy` | Foreground operation, workflow dependency, or background review; producer-controlled |
| `created_at`, `expires_at` | Durable times; optional expiry determined by producer policy |
| `state`, `decision`, `application` | Separate human disposition and domain application status |
| `supersedes` | Optional prior request replaced by a changed proposal |

The native caller supplies execution context; the model cannot set IDs,
identities, workspace scope, grant duration, handler or wait policy. The subject
includes the relevant canonical paths, exact arguments, working directory and
config/target revision. Changes to relevant execution policy or identity
invalidate a pending request. Revalidation happens before application too.
A path approval does not make arbitrary file contents immutable.

Wire commands:

```text
create(prepared_subject, trusted_context, idempotency_key)
get(id, caller)
list(scope_filter, cursor, caller)
decide(id, expected_revision, option_id, feedback?, caller)
withdraw(id, expected_revision, caller)
subscribe(scope_filter, after_sequence, caller)
```

Caller authority is derived from the authenticated/admitted transport and
injected by trusted code. It is not a deserialized `human: true` assertion.
Standard ACP responses retain whether the admitted client is delegating a
decision under explicit user policy; automatic client answers must not be
mislabelled as an individually verified button click.

Invalid options, expired/replaced requests, foreign audiences and mismatched
revisions fail without effects. Duplicate decisions selecting the same option
and feedback return the original result; competing decisions conflict. Every
request mutation atomically appends an event in the approval database.

## State and persistence

Human disposition is one of:

```text
pending -> approved | rejected | withdrawn | expired | superseded
```

Terminal disposition never changes. Request edits create a replacement rather
than rewriting something another client may already be reviewing. The service
allows multiple requests per scope; a later request does not overwrite an
unrelated pending request.

Application is separate:

```text
not_required
not_started -> applying -> applied | failed | unknown
```

Rejecting a workflow or memory proposal also has a domain application, so its
receipt is tracked. A human approval followed by a failed config reload remains
an approved decision with failed application; UI must not report success.
`unknown` means effects cannot be established, not that retry is permitted.
Deterministically retryable adapters can acquire a new fenced application
attempt after reconciliation; authorization is revalidated each time.

Use the existing native Turso adapter for `~/.otto/approvals.db`, containing
requests, terminal decisions/application attempts and monotonically sequenced
events. Creation deduplicates by producer/store/subject/version, so repeated
notifications, reconnects and recovery do not create duplicate prompts.
Decision CAS and application claims work across processes. Live watch channels
only accelerate delivery; the database and persisted sequence are authoritative.

Producer commits and approval creation can be in different databases. A memory
candidate or workflow step is first durably prepared, then registered through
an idempotent request key. Startup and periodic reconciliation enumerate all
prepared subjects lacking registration, with pagination. Reconciliation uses
the same creation path and publishes missing requests; it never fabricates
human decisions. Approval registration failure prevents dispatch and remains
visible as a producer failure/retry condition. Missing producer stores leave
requests unavailable, not silently approved or lost.

## Applying a decision without false atomicity

1. Persist the decision and event before returning it or releasing a waiter.
2. Claim its application with an attempt ID and owner fencing token.
3. Revalidate scope, subject and relevant policy. Only then cross the effect
   boundary, under the session/task lease where applicable.
4. Commit the domain mutation and its `(approval_id, revision)` receipt in the
   same domain transaction when the domain supports transactions.
5. Record the observed application result in the approval journal and publish
   it. Recovery can reconstruct this record from the domain receipt.

This is not a distributed transaction. Native memory review must commit the
candidate transition, resulting record/tombstone and receipt together. Workflow
must commit its decision projection, dependency transition and receipt together.
Old review/response endpoints route through this service; they cannot commit a
second decision behind its back.

Config and skill files require receipts with before/after content versions and
the existing safe writer/ownership checks. Recovery distinguishes unchanged,
exact authorized result, and conflicting content. Config save and runtime reload
are separate recorded steps; failures reconcile/roll back only when the current
file still matches this attempt. Do not overwrite a concurrent user's change.
Do not record a file mutation as applied merely because a rename was attempted.

Bash, arbitrary MCP effects and other external actions cannot promise exactly
once execution. Persist authorization consumption/dispatch intent before start.
Use the existing operation ID, session operation ledger and effect certainty.
After a crash in the effect window, settle as unknown unless an authoritative
receipt establishes the outcome. Never automatically replay an unknown write.
Cross-process claims must reject stale owners before dispatch; a takeover must
not assume an already-started external process has been stopped merely because
its lease expired.

Withdrawal only wins while disposition is pending. Cancellation of an approved
operation is recorded separately and checked before dispatch. It does not erase
the approval or undo prior effects. A permanent grant is stored by its domain;
revoking that grant is a distinct operation, not withdrawal of its old request.

## Agent waiting and model-initiated confirmation

### Model discovery and initiation

Expose a real `request_approval` tool in the active runner definitions. Its
schema accepts `title`, `reason` and exactly one of `proposal` (confirmation)
or `subject_ref` (a prepared domain operation the current session may submit
for review). A reference is resolved and revalidated by its domain adapter;
it is not an executable callback, a raw grant or an arbitrary object ID.
Requesting an existing subject reuses its request rather than creating a
second prompt. The tool returns a typed decision and optional human feedback.
The service binds it to the originating logical operation and stores an
immutable proposal or subject binding. Domain application remains the domain
adapter's responsibility.

The system prompt describes what approval does, how to call it, how decisions
are returned, and how it differs from ordinary discussion and existing tool
permissions. `otto_help` capabilities must discover it from the actual runner.
The prompt also describes pending-query/withdrawal tools when available.
Capabilities and producer references visible to the model are session-scoped;
unavailable tools/modes must not be advertised as usable. Tool schema delivery,
prompt delivery and actual runtime support are separately verified.

For discretionary consent, the model chooses whether and when to initiate:
plan acceptance, an approach whose suitability needs user judgment, or an
additional action outside the user's agreed scope. Do not introduce an automatic
UI heuristic or keyword detector that turns every plan into an approval.
Prompt instructions encourage requesting consent only when needed and avoiding
repeat requests for unchanged actions already authorized by the user. This is
guidance for the model's judgment, not a claim that every natural-language
authorization requirement can be mechanically enforced.

Mandatory domain checks are a separate obligation. Sandbox escalation,
non-human memory mutation and explicit workflow gates still require valid
authorization even when the model never calls `request_approval`. Models can
initiate these earlier through prepared subjects or permission-bearing tool
calls; omission cannot disable their preflight checks. A confirmation never
manufactures the authorization a domain requires.

### Waiting and continuation

For operations requiring authorization, the native tool executor prepares it
before effectful dispatch,
publishes a typed wait event and awaits the service. The exact prepared call is
executed after authorization; no text parser or synthetic 'retry this command'
provider prompt carries permission. Execution context is threaded from the
existing `ToolCall.operation_id` through the native registry/tool boundary.

The core loop observes a typed batch-control outcome in addition to the ordinary
tool result. While waiting, it neither calls the provider again nor dispatches
later tools. After `request_approval`, all remaining calls from that same
assistant message are settled as not started; a new provider step must use the
human decision/feedback before planning additional actions. Rejected, withdrawn,
expired or invalidated authorizations likewise abandon that batch. An approved
ordinary operation can complete before sequential dispatch continues, with each
subsequent call independently checked. Previously executed calls are not undone.

The wait is observable as `waiting_approval`, with request IDs; it does not hold
database locks or consume a provider request timeout. Explicit workflow/run
deadlines continue to apply. Approval expiry is its own producer-controlled
deadline, checked transactionally at decide/claim as well as by a timer. Memory
review and workflow gates have no implicit ten-minute timeout; short-lived
execution/config requests default to the existing ten minutes.

The session retains foreground admission while waiting. Ordinary task messages
queue. Users can query, discuss, reject, withdraw or revise the pending proposal
through a restricted approval dialogue, bound to an explicit request ID. With
multiple requests, the user selects one instead of an implicit 'latest'. The
dialogue has no execution/grant tools; replacement proposals supersede old ones
atomically. Only the original session writer appends dialogue outcomes and the
eventual decision to history.

Pending confirmations survive restart. They can still be decided, but their
continuation resumes only under the restored session lease and append-only
operation recovery. Approved confirmations are never treated as a replayable
shell grant. Execution authorizations from an interrupted turn become invalid
for dispatch; renewed execution requires a fresh request. Background memory,
skill and workflow subjects recover independently of a foreground turn.

## Producer-specific contracts

| Producer | Subject and application |
| --- | --- |
| Bash elevation | Exact prepared call and workspace; consume once before unconfined dispatch; retain platform restrictions |
| Sandbox read path | Canonical path, typed config preview and scope; save/reload successfully before the original confined call resumes |
| Sandbox network/exclusion | Exact network policy or parsed exclusion pattern; disclose persistent widening; model cannot add these as confirmation metadata |
| Memory | Candidate action, scope, base revision and proposed content; all non-human writes/forgets require review; direct complete human requests use recorded direct consent |
| Reflection skills | Vetted action, owned target, exact content/diff and base digest; human approval does not bypass rule/model vetting or ownership checks |
| Workflow | Run/definition identity, step and approved prompt; rejection cancels according to the existing DAG contract; other independent approvals can coexist |
| Profile/config | Validated typed change and base file version; disclose restart-required behavior and expose application errors |
| Subagent | Child operation/request identity and parent audience; cancellation closes only associated foreground requests; no inheritance of blanket authority |
| MCP tools | Server identity, tool, exact args and configured consent policy; server-provided read-only hints alone cannot confer write authority; unclassified calls require authorization unless explicitly preauthorized by the user |
| Conversation | Exact proposal; approve/reject with feedback; confirmation conveys no executable grant |

MCP tool policy is explicit and owned by Otto, not guessed by the model. Existing
configured preauthorization must be displayed/documented as user policy, not
invented at upgrade. Server elicitation requests are validated, scoped to the
originating call and routed through a separate information/consent adapter; it
must not expose secrets through a form or imply OAuth completed from URL consent.
Advertise only elicitation modes actually implemented on each transport.

## HTTP, native UI and chat delivery

Provide a common HTTP surface:

```text
GET  /v1/approvals                 # scoped snapshot + event cursor
GET  /v1/approvals/{id}            # authorized details/current state
POST /v1/approvals/{id}/decision
POST /v1/approvals/{id}/withdraw
POST /v1/approvals/{id}/message
GET  /v1/approvals/events?after=…  # scoped resumable SSE
```

Snapshot and cursor come from the same read transaction. Changes after that
cursor replay before live subscription. If retention has removed the cursor,
return an explicit resync response. Filter payloads before delivery; a global
sequence number is not permission to see global request content.

Every frontend must display a pending queue, exact details and authorization
duration, approve/reject and optional feedback, discussion/withdrawal, and
decision/application outcomes. A compact preview cannot conceal truncated
details: expose the complete review content before approval. Default selection
is non-approving. Reconnect queries authoritative state, including requests
created outside a turn. No separate Bash-only or memory-only callback logic.

Chat audiences are explicit platform/chat/admitted-user bindings, resolved by
trusted session bindings; do not broadcast workspace requests into every chat
that has ever used the workspace. Keep existing sender/chat admission checks.
Callback tokens map to approval ID, revision and option; revalidate audience
and current state at decision time. Persist recoverable token bindings or use
verifiable opaque tokens so restart does not create unsafe orphan buttons.

Multiple authorized surfaces may display one request. One committed decision
closes all copies. Card delivery is retryable and deduplicated by request/version;
the approval journal remains pending if sending or editing a card fails. An
adapter retries/reconciles presentation and exposes delivery failure rather than
ending the task as if approval were denied. Bound-ID commands provide a fallback;
bare `/allow` is valid only when exactly one matching foreground request exists.

UI dismissal and connection loss are not human rejection. Explicit turn cancel
closes its pending foreground approvals; memory/workflow reviews stay pending.
Durable requests can be decided after reconnect, subject to scope and expiry.

## ACP v1 adaptation

Keep the current protocol version; an ACP version upgrade is not required for
this component. Verify negotiated schema/client support rather than assuming
new documented optional fields are in the installed dependencies.

| Semantic operation | Mapping |
| --- | --- |
| Foreground tool authorization | Standard `session/request_permission`, real originating tool-call ID and truthful option kinds |
| Model plan confirmation | `elicitation/create` form when the client advertises it; accept/decline/cancel map to the exact confirmation |
| Persistent queues, background approvals, discussion and cross-client state | Negotiated `_otto/approvals/*` extension |
| Unsupported confirmation client | Explain pending request and offer a supported authenticated decision surface; never assume assent or disguise it as Bash permission |

Define `_otto/approvals/list`, `get`, `decide`, `withdraw`, `message` and a
cursor-based `watch` request returning bounded event pages. Advertise versioned
capabilities in `_meta.otto.approvals`. `watch` and list must obey the same scope
rules as HTTP. Connector uses this unified extension for its queue; standard
permission responses from other ACP clients still enter the same service.

Map request IDs separately: Otto approval ID, tool-call ID, JSON-RPC request ID
and chat token have different lifetimes. `_meta.otto` may carry correlation and
presentation data, never additional unvalidated authority. Only domain-advertised
options are accepted; do not identify an option by its label or hard-coded
`allow_once` string. Persistent effects use truthful `allow_always` presentation.

Local stdio, authenticated TCP and `--attach` use identical mapping and service
contracts. Another client's decision closes an outstanding ACP interaction
through the existing negotiated cancellation handling; late answers return the
authoritative state/conflict without a second effect. Transport disconnect
releases that client waiter, not the durable request. Explicit ACP turn cancel
settles foreground requests as withdrawn and respects protocol cancellation.
Elicitation additionally binds the receiving connection and verified identity;
its response correlation cannot be reused by another connection. A separately
authorized Otto decision surface may resolve the domain request, closing that
interaction without fabricating an elicitation response.

Protocol references checked for this design:
[permission requests](https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission),
[elicitation](https://agentclientprotocol.com/protocol/v1/elicitation), and
[extensibility](https://agentclientprotocol.com/protocol/v1/extensibility).

## Privacy, errors and observability

Persist sanitized operational subjects and required immutable plan/skill review
artifacts with owner-only permissions. Reject credential-bearing proposals;
never persist provider/bot secrets or environment values. Memory content stays
in its canonical candidate store: persist identity/version/digest, hydrate only
for authorized review, and invalidate unavailable/changed details. Forget/purge
also removes any retained review artifacts; event history retains non-content
IDs and outcome metadata, not an additional copy of forgotten memory text.

Decision audit records trusted transport, admitted actor, option and time.
Feedback/proposal retention follows session privacy rules and is excluded from
ordinary logs. Log IDs/source/state/latency/delivery failures, not raw commands,
arguments, memory contents or review text. Metrics distinguish waiting,
decision, application failure and unknown effects. Loss of approval storage
fails closed before dispatch and reports an actionable unavailable error.

Typed API errors include invalid input, unauthorized scope, unavailable details,
not found, revision conflict, already decided, expired, service unavailable and
application failure. Pending-list failures must not appear as an empty list.

## Migration and delivery

Deliver the whole contract, including all frontends and recovery. Sequencing
below is integration order, not permission to ship disconnected partial behavior.

1. Add neutral contracts, native journal, injected service and domain receipt
   boundaries; update the core execution/batch/wait contracts.
2. Migrate producer state and old decision entry points into the service. Import
   existing pending memory/workflow subjects idempotently; preserve append-only
   history. Historical decisions remain historical and must never trigger work.
3. Integrate model confirmation, restricted dialogue, reflection skill review,
   config previews and explicit MCP authorization/elicitation policy.
4. Route TUI/CLI, HTTP/Web and every ACP backend through the common contract;
   replace connector's separate permission/memory callback paths.
5. Validate recovery, privacy, upgrade and complete user flows before release.

Memory schema v2 has a strict fingerprint. Receipt/schema additions need an
explicit backed-up migration, not edits to the old manifest. Existing approval
and review command/API spellings remain compatibility adapters where their
semantics match; legacy ambiguous decisions fail with a specific-ID instruction.
Do not rewrite historical session entries. Update repository maps, canonical
development contracts, user manual, wire/OpenAPI fixtures and architecture guards
in the implementation change. This proposed document claims no shipped behavior.

## Acceptance evidence

All checks are offline and credential-free. Behavior tests must fail if the
approval is bypassed, lost, applied twice or delivered to an unauthorized user.

- Every producer creates the correct immutable subject; existing direct human
  consent and explicit preauthorization do not generate redundant prompts.
- Multi-process conflicting decisions produce one winner; duplicate responses
  reuse receipts; stale versions/options/actors/owners cannot dispatch effects.
- Failure injection covers domain creation before registration, decision before
  application, claim before effect, domain commit before journal update, and
  interruption in external execution. Recovery neither loses requests nor
  repeats unknown writes.
- Plan confirmation pauses provider and tool dispatch; same-batch operations
  are not executed after confirmation/rejection; revised plans invalidate cards.
- Both providers receive the actual approval tool schema and matching system
  prompt; installed help lists only available capabilities. Models can call the
  tool for discretionary consent without a frontend trigger. Prepared-subject
  calls deduplicate and respect scope; skipping the tool cannot bypass mandatory
  domain authorization. Tests verify the runtime boundary without claiming to
  prove every real-model judgment from a scripted provider.
- Every frontend receives foreground and background requests, shows persistence,
  supports feedback/discussion, closes after cross-client decisions and restores
  queues after reconnect/restart. Delivery failures leave observable pending work.
- Child requests retain identity and parent audience; cancellation, workflow
  deadlines and multiple concurrent/background approvals have distinct effects.
- ACP stdio/TCP/attach tests cover standard permission, elicitation capabilities,
  extension negotiation, cursor replay, unsupported clients and late responses.
- Memory revision/secret guards, skill vetting/ownership, config CAS/rollback,
  platform restrictions, operation certainty and lease fencing remain enforced.
- Retention/resync, detail access and purge tests prevent scope leakage and
  retained copies of forgotten memory. Historical imports never replay effects.

Run focused contract/domain/frontend checks first, then `make check-fast`,
`make connect-check`, `make check` on supported macOS, and the Linux CI gate
`make check-linux`. Include changed wire/OpenAPI fixtures and wasm boundary checks.
Rebuild/install and restart the connector before claiming installed chat behavior.
