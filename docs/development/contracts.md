# Development contracts

Previous: [Testing](testing.md) · [Documentation index](../README.md) · Next: —

## Secrets, safety, and documentation

- Never add `--api-key`; API keys come only from environment variables, and ChatGPT credentials only from `otto login`.
- Never put raw API keys, OAuth tokens, or auth headers in TOML, JSONL session fixtures, logs, docs, or tests. Redact sample values in errors and examples.
- `read`, `grep`, `find`, `ls`, `write`, `edit`, and `skill` must reject workspace/skill-directory escapes after canonical path and symlink validation.
- Do not describe `bash` as always unsandboxed. Otto defaults to macOS Seatbelt; only explicit sandbox `off` is unsandboxed.
- The chat connector reads its Telegram bot token and Feishu app secret from the environment variables that `connect.toml` names; a secret key in that file fails the config load. Its errors and logs never contain those secrets, and it does not log message text (the Feishu adapter drops the SDK's debug and info logs, which carry event payloads and the connection URL).
- Never put secrets in skill files; skill content is user- or repository-provided instruction text of the same class as `AGENTS.md` and `CLAUDE.md`.

Keep README limited to implemented, tested behavior; list unsupported behavior
under Limitations. Do not list roadmap stages or planned providers. Keep command
examples aligned with the actual CLI flags in `crates/otto/src/cli/flags.rs`.
Document the config, session, and safety behavior that tests enforce today,
not aspirational behavior.

Use small, focused commits with imperative subjects, for example
`feat: add OpenAI-compatible streaming` or `docs: document the ChatGPT sign-in
flow`. Before committing a Rust change, run only `cargo fmt --all`; CI runs the
build and test gates. Confirm that the working tree contains only intentional
changes.

### Unified approval contract

`otto-core::approval` owns neutral requests, choices, scopes and separate
human/application states. `otto::approval` stores them in `approvals.db`; the
same decision authority is injected into the parent and child native registries,
HTTP, ACP and terminal surfaces. Models discover `request_approval` in actual
runner definitions and system guidance, and choose when conversational consent
is needed. Models can query/withdraw but cannot decide an approval. Canonical
`ApprovalSettings { policy, reviewer, revision }` separates sandbox permissions
from approval policy. `/auto manual|auto|yolo` resets both controls to a preset;
`/permissions` and the revision-checked policy APIs expose separate controls.
Only admitted user actions may change settings/rules. Confined operations run
directly. `never` rejects requests needing additional authority; AUTO reviews
eligible session-owned requests; explicit checkpoints, workflow human gates,
questions and security permission changes still require human action. A child
inherits its root session's live settings; unrelated/workspace requests cannot
borrow them. User/managed rules precede isolated review, with deny > ask > allow;
complex shell programs cannot match a simple argv-prefix rule.

Native `approval/reviewer.rs` defines the isolated `ReviewProvider` and immutable
`ReviewContext`; the service owns coordination, journal/leases and decisions.
The provider assesses an exact operation with root user evidence and accepted
instruction versions, never executes it or changes approval storage. Generated
child prompts and assistant rationalizations are not user authorization. Review
input/output limits are 64 KiB/2 KiB, the deadline is 30 seconds, and concurrency
is capped at four. Missing/oversized context and provider failures require a user
decision. Settings, evidence/policy digests and request revisions are checked
again at decision commit; domain targets are rechecked before dispatch.

`Request.review` is additive; absent metadata denotes a legacy human request.
Queues expose only human-required requests; snapshots report reviews and
application failures separately. Audit events retain queued/reviewing transitions
and decision actor/transport. Review ownership is independent of application
ownership: an expired review lease can be retried, but an unknown operation
effect cannot. The version 4 database migration makes a consistent backup,
preserves history and portable transfer metadata, and keeps old pending requests
human-required. Creation and decision remain distinct journal events. See the
[smart approval contract](../specs/2026-10-08-smart-approval.md).

Authorization is prepared before tool dispatch, bound to the logical operation,
exact arguments and relevant domain version. Decisions/events commit together.
An application claim has a process identity; maintenance must not take over a
live owner's work. Domain handlers reconcile memory candidates, workflow gates,
config previews and vetted skill artifacts through their canonical receipts.
External actions with an uncertain effect are not replayed automatically.

Confirmation/rejection forces a new provider step: later calls in the same
assistant batch receive `NotStarted` results. Skill proposals never write when
approval service composition is missing. Memory review text stays in the
candidate store; the approval journal retains references and digests only.

HTTP exposes `/v1/approvals`, per-request decisions/withdrawal and resumable
changes/events. ACP uses standard permission requests for tool authorization,
negotiated elicitation for confirmation, and `_otto/approvals/*` for durable
queues. `Request::presentation_origin` is trusted passive-delivery metadata,
not authority: approval `Scope` still decides who may list or decide a request.
Parent-turn producers snapshot the current exact `schedule::Origin`; originless
background requests remain explicitly discoverable but are never inferred as a
chat broadcast. HTTP notices, ACP metadata and connector cards preserve that
origin through attach mode, and the connector presents only an exact admitted
chat match. Connector cards and commands bind the exact request/version;
feedback returns to waiting models and non-TUI clients, while the TUI deliberately
has no feedback field. MCP calls pass the same native authorization boundary;
unsupported inbound elicitation modes are not advertised and cannot be
interpreted as authorization.

The TUI renders every presenter as a centered panel over the normal session
shell; no feature presenter may replace or clear the whole terminal. Approval
actions share one horizontal row, select Approve/Accept on the left by default,
and use Left/Right to move between decisions. Read-only documents are scrollable
content, not focus controls: clicking one must not take focus from the selected
action. Approval presenters close immediately after either successful decision
and present pending requests one at a time in creation order; an undecided `Esc`
dismissal remains pending and can be reopened from `/approve`. Memory
availability refreshes that shared approval queue instead of opening a separate
review surface.

Memory proposal routing lives in the candidate's internal
`otto.presentation_origin` metadata until the corresponding approval is
published. Accepting a candidate removes that internal key before the memory
record is written. Existing candidates have no origin, remain explicitly
reviewable, and are not passively broadcast.

External chat notification routes are exact destinations, not session or
workspace subscriptions. Originless session-open memory notices stay available
to local and explicit API readers but do not fan out through the connector.
Wake replies keep their separately documented session-subscriber behavior.

Design: [unified approval](../specs/2026-10-04-unified-approval.md).

### Agent self-help and approval conversations

`tool::otto` reads installed help from the embedded canonical user manual and
uses current runner definitions for capabilities. `app::approval_control`
reuses the provider with an isolated memory transcript and only help/query/
withdraw/queue tools while the original approval waits. It never grants or
runs task tools, and only the original runner writes dialogue context from
its inbox. A dialogue is bound to one session and original request ID;
withdrawal cannot affect a newer request. Protocol and frontend contracts:
[self-help and approval control](../specs/2026-10-03-agent-self-help-and-approval-control.md).

### Sandbox read approvals

Bash's optional `sandbox_read_path` requests persistent read access to one
absolute existing path, with a justification and `use_default` permissions.
The grant is durable authority, so it is always a user decision, whatever the
session approval mode.
`BashApprovals` binds its canonical path and exact command to the session's
latest approval ID, separately from unsandboxed command grants. The controller
applies the existing backed-up config amendment and sandbox reload before
granting a single confined retry. Reload failure rolls back the config; denial
and cancellation discard the pending request. A changed canonical path requires
a new request. The optional `read_path` on `approval_requested` distinguishes
read grants from elevation for attached clients. ACP advertises `allow_always`
and discloses persistence in the title; the connector routes it through its
existing cards and callback admission checks.

## Plugin execution contracts

`otto-core::config::plugins` owns package/config schemas; `agent::hooks` owns
an optional async hook port and session lifecycle state. Core invokes hooks
before tool dispatch so argument rewrites reach child write policy, tool
validation and exact-argument authorization. Native `plugin/` owns pinned
package discovery and immutable resource digests; `plugin/hooks.rs` owns
command execution and offline JSON Schema validation. Runtime wiring composes
existing skill/MCP paths and
injects the same plugin runtime into parent and child agents.

Before hooks may deny but cannot grant rights. Commands use the native sandbox
executor with bounded stdin, output and process-group cleanup. Project roots,
hook declarations and resources participate in executable trust; recheck at
effects and cancel running package commands when source or authority changes.
Preserve original operation facts across output transformations and append
argument/original-result audit entries before exposing rewritten results.
Hook effects make even read-only tool attempts ineligible for automatic
crash replay. Session hook state belongs to the shared session, never a
replacement runner. Graceful close owners await session-end hooks where the
frontend exposes asynchronous shutdown; synchronous close starts bounded
cleanup, and a process crash makes no delivery guarantee.

Protocol and examples: [Hooks and custom tools](../manual/memory-and-extensions.md#hooks-and-custom-tools).

## Agent implementation contracts

Execution diagnostics are out-of-band: core emits typed metadata through an
optional injected observer and native startup supplies private rotating files.
Never pass request/response/tool bodies or raw errors to the observer, copy
ordinary stderr lines into the diagnostic sink, or inject diagnostic records
into model history. `Provider::complete_observed` defaults to the original
provider method; transport adapters and timeout wrappers preserve observation
without changing stream events, request serialization or cancellation behavior.

A tool executor may request a nonblocking turn yield through `yields_turn` only
for a successful, completed tool execution. Core settles and persists the whole
batch first. Failed calls, approval replanning, queued notifications and steering
win over a yield; later mailbox delivery uses the existing frontend wake path.
Custom executors default to not yielding. Never infer scheduling control from
model-visible output text. Native wrappers must preserve the declaration while
registry outcome/authority checks remain in force.

Apply these requirements to every feature, fix, and refactor:

- Keep agent instruction files short navigation maps and rulebooks. Put detailed contracts next to their owning code or in this development guide, and link to one canonical source instead of copying it across agent instructions.
- Make changes easy to locate: use consistent domain names, cohesive packages, explicit entry points, and adjacent tests. Update the task map when responsibilities move or new packages are introduced.
- Keep dependencies explicit and one-way. Put shared behavior in its owning layer and inject dependencies at the composition root; avoid hidden global state and frontend-specific copies of shared use cases.
- Make contracts readable from types and focused documentation: define valid states, ownership and mutation, concurrency and cancellation, error behavior, and compatibility where relevant. Validate data at trust boundaries.
- Extend existing patterns with the smallest cohesive change. Add interfaces only at real consumer boundaries; do not add speculative frameworks, registries, or configuration for future implementations.
- Turn important invariants into executable checks. Update architecture guards and contract tests when boundaries change; failures should identify the violated rule and where to fix it. Keep focused checks fast, offline, and deterministic, and preserve full acceptance coverage.
- Keep code, schemas, tests, and canonical docs consistent in the same change. Remove stale instructions or mark superseded designs as historical; record durable rationale in the repository rather than relying on chat history.

### Capability descriptions

The model learns what Otto can do only from these sources. Do not add a
separate capability catalog.

- Written by hand: each tool's `description` and input-schema field
  descriptions; the policy sections in `system_prompt_for`
  (`crates/otto/src/runtime/prompt.rs`), each rendered only when its tool is
  registered; the user manual, which `otto_help` serves to the model; and
  `EXCLUDED_CHILD_TOOLS` (`crates/otto/src/subagent/runner.rs`).
- Generated from the registry, never edited by hand: `<available_tools>`, the
  `otto_help` `capabilities` topic, and `capability_update` notices.

A description is short and has one reading. It states:

- the capability: what the tool does, and when to use it instead of a related
  tool;
- inputs: each parameter's meaning, type, allowed values or range, default,
  and whether it is required;
- outputs: what a successful call returns and what a failed call returns;
- limits and effects: counts, sizes, durations, persistence, and effects that
  need approval.

Use one term per concept and concrete values ("at most 8 timers, 1 to 3600
seconds"), not adjectives ("a few", "short"). Leave out text that repeats the
schema or does not change how the model calls the tool.

Adding or changing a capability updates, in the same PR:

1. the tool's description and schema field descriptions;
2. its policy section in `system_prompt_for` when its use depends on other
   tools, ordering, approval, or the session lifecycle, with a prompt test
   that the section is present when the tool is registered and absent when it
   is not;
3. the descriptions and policy sections of related tools that should name it
   (for example, `remind` names `schedule`);
4. `EXCLUDED_CHILD_TOOLS`, when sub-agents must not have it;
5. the user manual.

The tool name must pass `otto_core::tool::safe_prompt_tool_name`; otherwise it
is left out of `<available_tools>` and of `capability_update` notices.

Removing a capability deletes, in the same PR, its policy section and every
mention of its name in other tools' descriptions and schemas, other policy
sections, `EXCLUDED_CHILD_TOOLS`, and the user manual, including its example
agent definitions. A remaining mention directs the model to a tool that does
not exist. Check with `git grep -w <name> -- crates docs`: each remaining
match is a historical spec, a test that asserts the tool's absence, session
compatibility code, or an unrelated use of the same word. Renaming is a
removal plus an addition.

The PR body lists the added, changed, and removed tool names and the updates
made for each. A PR that changes a capability without these updates is not
merged.

---

Previous: [Testing](testing.md) · [Documentation index](../README.md) · Next: —
