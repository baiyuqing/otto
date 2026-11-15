# Development contracts

[Development guide](../development.md) · [Architecture](architecture.md) · [Testing](testing.md)

## Secrets, safety, and documentation

- API keys come only from environment variables; never add `--api-key`.
  ChatGPT credentials come from `otto login`. Connector secrets come from the
  environment variables named by `connect.toml`, not literal config values.
- Never put credentials/auth headers in config, fixtures, sessions, logs, docs,
  tests or skills. Redact errors/examples. Connector logs must not expose chat
  messages or SDK payload/connection URLs containing secrets.
- New session recording copies use best-effort credential detection in addition
  to known-value redaction; live tool arguments stay separate. See the
  [recording boundary and rule provenance](credential-detection.md).
- File tools reject workspace/skill-directory escapes after canonical-path and
  symlink validation. Reuse pinned filesystem operations.
- Bash goes through the native sandbox and starts in the selected workspace;
  only explicit `off` is unsandboxed. Never describe Linux as sandboxed.
- Skill/repository content is untrusted instruction text, not security authority.
- Document implemented/tested behavior only. Keep examples aligned with CLI
  flags, schemas and tests; remove superseded instructions. Follow
  [verification](workflow.md#local-verification) and [changelog](workflow.md#changelog)
  rules rather than duplicating them in other guides.

### Unified approval contract

`otto-core::approval` owns neutral requests, choices, scopes and separate
human/application states. Native `approval` owns `approvals.db` and one decision
authority shared by parent/child registries, HTTP, ACP and terminal clients.
Models may request, query or withdraw approvals; they cannot decide them.

Approval content is **not secret-scanned**, including titles, reasons, details,
feedback, review context and reviewer output. It may be displayed/persisted;
callers must not include real secrets. Structural/size validation, permissions,
policy and human checkpoints remain enforced. Non-approval secret protections
are unchanged; `Service::protect_secrets` is a compatibility no-op, not a toggle.

`ApprovalSettings { policy, reviewer, revision }` separates sandbox permissions
from policy. `/permissions manual|auto|yolo` applies presets; `/permissions
policy|reviewer` and revision-checked APIs expose separate controls. Only
admitted user actions may change settings/rules. Children inherit their root's live settings; unrelated
workspace requests cannot borrow them.

- Confined operations execute directly. `never` rejects additional authority.
  AUTO reviews eligible session-owned operations; explicit checkpoints,
  questions, workflow human gates and security permission changes still require
  a person. User/managed rules precede isolated review: deny > ask > allow.
- Shell prefix matching parses every simple command with tree-sitter-bash.
  Allow requires a fully static list/pipeline with every segment matching;
  deny/ask may match any segment. Do not authorize a compound command using
  only its first executable, or confuse sandbox exclusion with effect approval.
- `approval/reviewer.rs` owns the `ReviewProvider` contract and immutable `ReviewContext`; `runtime/approval_review.rs` implements it with an isolated agent.
  It assesses the exact operation against root user evidence and accepted
  instruction versions; generated child prompts/rationalizations are not
  authorization. It never executes operations or mutates approval storage.
- Review limits are 64 KiB input, 2 KiB output, 30 seconds and four concurrent
  reviews. Missing/oversized context or provider failure requires user action.
  Recheck settings, evidence/policy digests and revisions at decision commit,
  and domain targets before dispatch. Legacy requests without review metadata
  remain human-required.
- Prepare authority before dispatch, bound to logical operation, exact arguments
  and domain version. Commit decisions/events together. Application claims use
  process identity; never steal a live owner's work. Review lease expiry allows
  re-review, not replay of an unknown operation effect.
- Reconcile domain effects through canonical receipts (memory, workflows,
  config previews, vetted skills). Never automatically replay uncertain external
  actions. Creation/decision and review/application states remain distinct.
- Confirmation/rejection forces a new provider step: later calls in the same
  batch get `NotStarted`. Skill proposals fail closed if approval composition is
  absent. Memory review bodies stay in candidates; journal references/digests
  are not copies of review text.

HTTP exposes durable requests, decisions/withdrawals and resumable events. ACP
uses permission requests for tools, negotiated elicitation for confirmation and
`_otto/approvals/*` for queues. MCP uses the same native authorization boundary;
unadvertised inbound elicitation is never authorization.

`presentation_origin` is passive delivery metadata, not authority: `Scope`
controls listing/decision. Producers snapshot exact `session::origin::Origin`; an
originless background request is discoverable, not an inferred broadcast.
Connector cards/commands require an exact admitted chat and request/version.
Feedback reaches waiting models and non-TUI clients; TUI has no feedback field.

TUI presenters are centered panels over the session, never full-terminal
replacements. Decisions share a horizontal row, default to left-side
Approve/Accept and use Left/Right. Read-only document clicks do not steal action
focus. Successful decisions close immediately; pending requests appear in
creation order. Esc leaves a request pending and `/approve` reopens it. Memory
availability refreshes this shared queue, not a separate approval surface.

### Agent self-help and approval conversations

`tool::otto` serves the embedded canonical manual and actual runner definitions.
`app::approval_control` uses an isolated transcript with only help/query/
withdraw/queue tools while the original request waits. It cannot grant or run
task tools. The original runner alone writes dialogue from its inbox. Bind the
dialogue to the exact session/request; withdrawal cannot affect a newer one.

### Sandbox read approvals

`sandbox_read_path` requests persistent read access to one absolute existing
path that the sandbox read denylist covers (otherwise the tool returns `path is
already readable; run the command without sandbox_read_path`), with a
justification and `use_default` permissions. This durable authority always
requires a person. Bind canonical path and exact command to the session's
latest approval, separately from unsandboxed grants. Apply the backed-up config
amendment and sandbox reload before one confined retry; reload failure rolls
back, and denial/cancellation discards the request. Changed canonical paths
require new approval. Attached clients disclose persistence (`read_path` metadata;
ACP `allow_always`) and preserve callback admission checks.

The `bash` capability description states: `Sandboxed commands read every path
except the sandbox read denylist (credential and personal-data locations)`. The
`sandbox_read_path` description states: `Request user approval to permanently
allow reading one absolute existing path that the sandbox read denylist covers,
before running this command inside the sandbox`. Update both texts when the
denylist model in `sandbox/read_denylist.rs` changes.

## Plugin execution contracts

Core `config::plugins` owns schemas and `agent::hooks` owns the optional async
port/session lifecycle. Hooks run before dispatch so rewritten arguments pass
child write policy, validation and exact-argument authorization. Native `plugin`
owns pinned discovery, immutable resource digests, sandboxed commands and offline
JSON Schema validation; parent and child agents use the same composed runtime.

Before hooks can deny, never grant authority. Bound command input/output and
clean up process groups. Project roots, declarations and resources participate
in executable trust: recheck at effects and cancel commands if source/authority
changes. Preserve original operation facts and append argument/original-result
audit entries before exposing transformed output. Hook effects prohibit automatic
crash replay even for an otherwise read-only tool. Session hook state survives
runner replacement. Async graceful close awaits session-end hooks; synchronous
close starts bounded cleanup; crashes promise no delivery. See
[hooks and custom tools](../manual/memory-and-extensions.md#hooks-and-custom-tools).
Plugin memory adds a versioned command/serde compatibility surface and a
registry downgrade constraint; see the retained
[protocol decisions](../specs/2026-10-20-memory-provider-plugin.md).

## Agent implementation contracts

Apply to every feature, fix and refactor:

- Keep cohesive domain names, explicit entry points, adjacent tests and one-way
  dependencies. Inject shared behavior at the composition root; avoid global
  state and frontend-specific copies of shared use cases.
- Define valid states, ownership/mutation, concurrency/cancellation, errors and
  compatibility in types and focused docs. Validate at trust boundaries. Extend
  existing patterns with the smallest cohesive change; add abstractions only at
  real consumer boundaries.
- Turn invariants into fast offline architecture/contract checks identifying
  the broken rule. Update code, schemas, tests and canonical docs together.
  Keep agent instruction files short navigation maps, not duplicate inventories.
- Release builds use `panic = "abort"`: a panic runs the panic hooks and ends
  the process without running `Drop`. Cleanup that must happen on a crash
  belongs in `cli::panic_cleanup`, not only in a `Drop` guard; register child
  processes with `otto_platform::child_fence::Children` so the hook can kill them.
- Execution diagnostics are out-of-band typed metadata, never request/response/
  tool bodies, raw errors, ordinary stderr or model history. Native private
  rotating files consume the observer; wrappers preserve serialization, stream
  events and cancellation. `complete_observed` defaults to normal completion.
- `yields_turn` applies only to successful completed execution. Persist/settle
  the whole batch first; failures, approval replanning, notifications and steering
  take precedence. Wrappers preserve the declaration, custom executors default
  off, and output text never controls scheduling.

### Lua plugin execution

Installed hook/tool Lua handlers follow the [Lua security contract](lua-plugin-security.md):
trusted code still has no host capabilities; limits and cancellation are cooperative,
not OS isolation. Engine/library exposure changes require security review and
adversarial regression coverage.

### Capability descriptions

The model learns capabilities from tool descriptions/schema fields, conditional
policy in `runtime/prompt.rs`, the embedded user manual and child exclusions in
`subagent/runner.rs`. Do not add a separate catalog. `<available_tools>`,
`otto_help` capabilities and `capability_update` notices are registry-generated.

Descriptions must be short, unambiguous and concrete: capability/when to use it;
input meaning/type/range/default/requiredness; success/failure output; limits,
persistence and approval effects. Avoid adjectives or redundant schema prose.

Adding/changing/removing/renaming a capability updates in the **same PR**:

1. Tool and schema field descriptions.
2. Conditional prompt policy and any related tools' descriptions/schemas.
3. `EXCLUDED_CHILD_TOOLS` when child availability changes.
4. Canonical user manual, including examples; `otto_help` uses it.

Tool names must pass `otto_core::tool::safe_prompt_tool_name` to appear in
available-tool/update guidance. Removing a capability removes every active
reference; renaming is removal plus addition. Check `git grep -w <name> --
crates docs`: remaining matches must be historical specs, absence tests,
compatibility code or unrelated words. The PR lists changed tool names and
corresponding description updates; missing updates block merge.
