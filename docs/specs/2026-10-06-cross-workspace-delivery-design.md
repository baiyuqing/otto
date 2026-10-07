# Cross-workspace delivery tasks

Status: **approved 2026-10-06; implementation in progress**

## Product contract

A source session may use a repository such as `diagram-design` as read-only context and, after a scoped human authorization, deliver work into a separate target repository such as `otto`.

The feature is a **delivery task**, not a mutable session workspace or multi-root filesystem grant:

- The source session remains bound to its canonical workspace and its existing file-tool root.
- Otto freezes a bounded, untrusted handoff packet of selected source context.
- The target must already pass ordinary workspace admission (`otto trust` or a configured server workspace root); delivery approval does not persist trust.
- The approval binds the source workspace, target primary checkout/common Git directory/HEAD, branch, target-local worktree path and handoff digest.
- After approval, Otto creates a new `TARGET/.otto/delivery-worktrees/<delivery-id>` worktree and builds a separate target `Builder`, sandbox, tool registry and session around that worktree.
- The delivery runner sees only the target worktree and frozen handoff packet; it never receives a live source root.
- GitHub credentials, networking, protected Git metadata writes, push, and PR creation retain their existing independent permission/approval boundaries.
- A failure or cancellation retains the worktree for inspection. Recovery never repeats external effects automatically.

## Key implementation decisions

1. Delivery is a dedicated native domain and must not be modeled as an ordinary subagent: ordinary subagents inherit the parent workspace and cannot start delivery tasks.
2. Delivery task records should be a separate persistence domain rather than mutate existing `tasks` storage/migration contracts.
3. Git preflight uses fixed argv, a hardened Git environment, bounded output, a deadline and no shell interpolation. Model-driven worktree creation must use an approval-scoped, constrained executor rather than a bare process.
4. Current `otto serve` workspace admission and target `cli::run::load_workspace` construction are reused after target revalidation. The target worktree, not its primary checkout, becomes the delivery runner root.
5. The initial public entrypoint is the approval-gated `start_delivery_task` model tool. It registers only for parent runners; no ordinary child receives it. Existing attached ACP authorization approval transport is reused; `cwd` remains a single-session workspace field.

## Delivery lifecycle

1. `start_delivery_task` validates strict arguments, target admission, Git identity, branch and the frozen handoff manifest; then it produces one nonpersistent authorization approval.
2. On allow, Otto re-runs the same validation and rejects any changed approval subject.
3. A durable receipt is written before/with the prepared worktree action, keyed by approval id so restart recovery never makes a duplicate worktree.
4. A dedicated target runner runs in the target worktree. It records target session/worktree/branch/result metadata and reports a completion notification to the source session.
5. GitHub work remains optional and explicit: only a confirmed returned PR URL is recorded as a PR outcome.

## Security invariants

- No source sandbox `read_paths` grant is made for the target, and no target sandbox grant is made for the source.
- The target must be an absolute canonical Git primary checkout; linked worktrees, non-Git directories and target paths outside admission are refused.
- Worktree paths are deterministic, target-local, non-preexisting and checked to stay beneath the selected repository.
- Branch names reject Git ref syntax hazards and never reuse an existing branch.
- The approval subject captures immutable repository and handoff facts; a mismatch before dispatch requires a fresh approval.
- Worktree setup remains distinct from generic unsandboxed Bash: it has fixed Git argv and cannot execute arbitrary command text.

## Phased implementation

1. Delivery primitive types: target Git identity, branch/worktree plan and bounded handoff digest.
2. Approval-gated tool with a narrow delivery service port and durable receipt.
3. Approval-scoped constrained Git worktree creation, target-builder lifetime ownership and target runner.
4. Delivery records, recovery and unified task views.
5. Server/attached ACP status exposure, canonical documentation and offline behavioral tests.

## Verification requirements

- Unit tests cover target/root/branch/path/handoff validation.
- Offline Git fixtures prove the primary checkout stays untouched and the delivery runner's tools reject source and primary-checkout paths.
- Approval tests prove deny is effect-free, allow is idempotent, and changed target facts invalidate the approval.
- No live GitHub, credentials, providers, or network access are used by tests.
- Run focused behavioral checks and `make check-fast` before PR creation.
