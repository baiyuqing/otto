# Sandbox-first unified smart approval

Status: approved for implementation, 2026-10-08. The user chose unified smart
review for execution and ordinary persistent writes and approved implementation.

## Behavior

Keep sandbox capabilities independent of approval policy and reviewer. The
default AUTO preset becomes on_request + auto_review. MANUAL is on_request +
user, YOLO is on_request + bypass. NEVER rejects operations needing additional
authority without prompting; it does not remove the sandbox. Ordinary confined
operations continue directly without a model review. Child tasks inherit the
root session's live settings without acquiring broader authority.

Use deterministic hard checks and explicit user/managed rules first, then review
remaining session-owned requests. deny beats ask, ask beats allow. No project,
model, skill or tool output may install permission rules. Pure argv-prefix rules
apply only to strictly simple commands; complex shell programs enter review.

The review scope includes Bash escalations, MCP, memory, skills, ordinary profile
settings, schedules and delivery. User questions, explicit approval checkpoints,
workflow human gates and security authority changes require actual user action
in AUTO. Repeated confirmations of an already authorized operation may be
recognized as already covered; never fabricate a new user choice. Workspace-only
or unrelated-session approvals cannot borrow a session's policy.

The main system prompt must stop requiring blanket confirmation for every new
action request. Missing material choices and user-requested checkpoints remain.

## Shared contract

`ApprovalSettings { policy, reviewer, revision }` is canonical. Policy is
on_request or never. Reviewer is user, auto_review or bypass. Missing settings
default to on_request, auto_review, revision 1. Legacy modes remain wire presets;
NEVER projects as legacy auto/automatic=true, with additive fields exposing the
actual policy and inactive reviewer. A legacy mode setter resets both controls
to its preset; advanced setters require the expected revision.

`Request.review: Option<ReviewMetadata>` is additive. ReviewMetadata contains
status (queued, reviewing, human_required, completed), optional assessment,
settings_revision, evidence_digest and policy_digest. An absent field means a
legacy human pending request, not new automatic authority. Assessment contains
risk (low/medium/high/critical), user_authorization (unknown/low/medium/high),
outcome (allow/deny/ask), and a concise safe reason. Existing approval and
application states and exact subject/version bindings remain authoritative.

`Config.approval` contains optional policy/reviewer/reviewer_profile and rules.
Each rule has one source, tool or command_prefix selector and an allow/ask/deny
decision. Only user/managed sources and admitted user actions can change them.

## Native ownership and integration seam

Approval Service continues to own journal, atomic decisions, claims, waits and
domain application. It accepts a registered isolated review provider and live
root-session authorization context. Native policy handles routing; the provider
only assesses an immutable request/context. No provider dependency belongs in
the store or portable core. One async coordinator covers all request producers.

Native shared seam, to be implemented in `approval/reviewer.rs`:

```rust
pub struct ReviewContext {
    pub trusted_user_messages: Vec<String>,
    pub instructions: Vec<String>,
    pub evidence_digest: String,
    pub policy_digest: String,
}
#[async_trait::async_trait]
pub trait ReviewProvider: Send + Sync {
    async fn assess(
        &self,
        request: &Request,
        context: &ReviewContext,
        cancel: &CancellationToken,
    ) -> std::result::Result<ReviewAssessment, String>;
}
```

Service exposes register_reviewer(workspace, root_session, provider),
set_review_context(workspace, root_session, context),
set_policy_config(workspace, approval_config), settings(scope), and
set_settings(scope, expected_revision, policy, reviewer). Methods use owned/Arc
values as appropriate; native service worker owns these methods. Runtime worker
owns the ReviewProvider trait/module and production provider implementation.

The coordinator hydrates canonical domain previews without replacing journal
references, takes an exclusive bounded review lease, and captures request,
settings, evidence and policy revisions. Context digest updates invalidate
stale assessments. Decision commit atomically checks persisted snapshots and
live settings; runtime policy/context updates must be synchronized with that
commit. Recheck the domain subject before dispatch. Provider outputs never
directly mutate approval rows or execute effects.

Default review is one tool-less request using the root session's provider/model;
reviewer_profile may select an existing configured profile. Budget: 30 seconds,
64 KiB input, 2 KiB output; global concurrency four. Secret-safe canonical
facts and required script/diff bodies accompany the exact operation. Assistant
rationalizations and generated delegation messages never establish user
authorization. Root original user evidence and fixed accepted instruction
versions survive compaction/restart; missing evidence becomes human_required.

Low/medium task-related actions allow; ordinary persistent changes need evidence
covering their content/scope. High risk requires exact strong user authorization
and bounded targets. Critical or explicit forbidden actions deny. Ambiguous
user-resolvable authority asks. Errors/timeouts route to human (NEVER rejects).
Three consecutive denials in a logical task stop further escalation attempts;
return the denial rationale and require a safer path or concrete user action.

## Persistence and recovery

Move approval DB user_version 3 to 4 in a transaction after a consistent backup.
Replace mode storage with canonical settings/revisions and migrate old boolean
and mode rows. Preserve portable storage-transfer import/export, opaque additive
envelope metadata, history, decisions and application claims. Older binaries
reject the new DB version. Rollback uses MANUAL; binary downgrade needs backup
restoration. Existing pending approvals remain human-required during migration.

Review ownership is separate from application ownership. An expired/dead review
owner may be retried because review has no operation effect; approved/applying or
unknown effects retain existing no-replay behavior. User decision, withdrawal,
mode tightening, expiry and review completion race under revision checks. An
unstarted automatic authorization must be revalidated after policy changes.

## Public surfaces

Add HTTP GET/PUT `/v1/sessions/{id}/approval-policy` and ACP
`_otto/approval_policy/get|set`, returning canonical settings plus legacy labels.
PUT/set accepts expected_revision and policy/reviewer. Preserve old approval-mode
routes. Add `/permissions` inspect and `/permissions policy on_request|never`,
`/permissions reviewer user|auto_review`; existing `/auto` presets remain.
Advertise the ACP extension explicitly and update command fixtures/descriptions.

Pending queues and approval_requested notices expose only human_required;
reviewing is a noninteractive status. Audit/event records retain queued/reviewing
and decisions with actor/transport identifying rule, reviewer, user or bypass.
Do not show Approve cards/modals during review. Keep separate human-pending,
reviewing and application-failure counts. CLI/TUI/HTTP/ACP/Telegram/Feishu and
otto-plus share backend queue semantics, never local risk decisions.

## Acceptance

Use offline fake-provider behavioral tests for direct confined work, AUTO
escalations and native writes, explicit human gates, child inheritance and
delegation provenance, missing/truncated context, secret redaction, never,
rule precedence/complex-shell rejection, mode/request/target races, cancellation,
expiry, reconnect, restart and no replay. Test DB backups/version rejection and
storage transfer. Native and browser APIs retain legacy compatibility.

Run focused tests, `make check-fast`, real `make rust-wasm-test` and connector
checks; verify real macOS Seatbelt behavior. Browser uses its own canonical
checks and pinned compatible core revision. Never run Otto `make check` or
`make check-linux` locally. Fake-provider tests establish execution correctness,
not real-model classification quality; report the latter separately. Deliver
paired focused PRs and installation/restart guidance after code review.
