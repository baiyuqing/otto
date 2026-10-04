# Chat context and reflection

Approved scope: make Otto's general-purpose identity and reflection
available consistently through `otto-connect`, including connections to
`otto serve`. This builds on the existing agent self-help and reflection
contracts rather than introducing a second agent or skill writer.

## Requirements

- Otto remains a general-purpose personal agent. Repository inspection and
  software delivery instructions apply to repository and software tasks.
- Connect supplies the current channel separately from the user's text. The
  server renders guidance for supported channels from validated metadata;
  clients cannot provide arbitrary system instructions through this field.
- Channel context applies to one turn. A shared or resumed session must use
  the current channel, and a later terminal/Web turn must not inherit it.
- Local ACP, attached ACP, and direct ACP TCP use the same behavior. Existing
  ACP clients without channel metadata continue to work.
- `/reflect [focus]` invokes the existing Reflector. `/skill generated` and
  `/skill revert <name>` expose its existing inventory and ownership-protected
  undo. These commands execute outside the model turn and remain subject to
  existing admission, cancellation, and permission rules.
- Background reflection results become visible in chat without being passed
  to the model as user requests or duplicated on every turn.
- Attached reflection cancellation matches the caller's exact operation ID.
  A delayed cancel must not cancel a later reflection or compaction. Existing
  HTTP callers can omit that ID and retain their existing behavior.
- Memory proposals still require human review. Skill writes retain evidence
  checks, source isolation, validation, model review, and ownership protection.
- `on_work` becomes the default automatic mode: after a completed agent turn
  with at least ten distinct successful tool operations, or a successful
  compaction, consider reflection using the existing pipeline. Pending
  approvals, errors, and cancelled turns do not qualify. Existing explicit
  `on_compaction`, `on_exit`, and `off` settings retain their behavior.
- Prefer revising an applicable owned skill to creating overlapping skills;
  skip learning when the transcript does not support a reusable procedure.

## Limits

Automatic triggers share the existing ten-minute session cooldown and
single-flight guard. The ten-operation heuristic counts one completed agent
turn; separately resumed approval continuations have their own counts.
External-content filtering is unchanged. Generated skills become active in new sessions;
this change does not introduce hot reload or a new skill-management framework.

## Acceptance

Offline tests must inspect provider requests to show that user text stays
unchanged, the current channel reaches system context, and channel information
does not leak into a subsequent turn from a different frontend. Test resumed
sessions and all three ACP connection modes. Command tests must show that
reflection commands do not reach the task model, inventory and undo preserve
ownership guards, and reflection notices reach chat once. Check the tool-count
threshold, successful completion, suppression, auto-mode compatibility, and
shared automatic cooldown. Run focused tests,
`make check-fast`, and `make check` before delivery.
