# Otto feature interaction and TUI redesign

Status: approved and implemented on 2026-10-06; local verification passed.

Source baseline: `cd43397`, inspected on 2026-10-06. The concurrent
`unified-human-input` worktree is separate work, not this baseline. This change
preserves its approved memory-review Accept and permission Deny defaults;
runtime admission and consent semantics remain in their owning services.

## Goal

A feature declares what the user can inspect, supply and decide. Shared
interaction code implements how users navigate, edit, submit, go back and
receive feedback. The terminal frontend supplies controls, layout and visual
style. Adding a feature must not require another independent key handler,
modal lifecycle or set of navigation instructions.

This replaces the existing ownership model throughout the TUI, rather than
adding a common widget beneath otherwise independent feature implementations.

## Current evidence

Paths below are relative to `crates/otto/src/`:

| Current owner | Mixed responsibilities |
| --- | --- |
| `tui/app.rs`, `App`, `handle_key`, `dispatch_line` | Composer, feature state, several modal states, raw keys, command parsing, backend queries and business dispatch |
| `tui/app.rs`, `PickerKind::title` | Feature labels contain instructions for terminal keys |
| `tui/context_view.rs`, `ContextView` | Context data, tree navigation, selection, detail scrolling and terminal text formatting |
| `tui/agents_view.rs`, `AgentsView` | Backend queries, filters, selection, detail navigation, transcript loading and raw keys |
| `tui/render.rs`, `draw` | Layout and feature-specific composition; independently chooses overlay precedence |
| `tui/mod.rs`, `apply_turn_key` | Another input path during a local turn, including approval-specific dispatch |
| `tui/attach.rs`, `on_key`, `Remote::block` | Another input path during a remote turn; synchronous view calls wait for HTTP through a helper thread |

The handler checks memory review, approval, help, context, agents and picker in
sequence. Rendering places approval in an inline slot and chooses other overlays
using another sequence. There is no single active-surface/focus model shared by
both. This is mechanism evidence, not a claim that every combination is a
reproduced visible bug.

Approval Esc currently denies; memory-review Esc closes without deciding.
Memory review uses alphabetic decision shortcuts in the baseline, while approval
also supports arrows and Enter. Context and Agents repeat list/detail navigation.
The related human-input work changes memory review to arrows and Enter with
Accept selected by default; this design preserves that approved default.

## Responsibility boundaries

```text
Domain services / app::Controller
    typed queries, operations, authoritative state and validation
                     ↕
Feature presenters
    data + available actions + semantic input requirements + feature transitions
                     ↕
Interaction host and reusable interaction state
    focus, navigation, editing, submission lifecycle, feedback, surface stack
                     ↕
Terminal adapter and rendering
    input decoding, concrete widgets, geometry, hit testing, theme, terminal I/O
```

These are responsibility boundaries inside the existing crates, not four new
crates or a remote UI-definition protocol.

### Domain and application

Keep sessions, approvals, candidates, task admission, authorization and operation
application in their current services. A UI action requests an operation; it
does not establish authority. Backend validation remains mandatory even when
the UI disables an action.

Queries return structured data rather than terminal-ready reports. Where a
shared use case currently lives in `cli::repl_commands`, move its application
work to the owning service and leave text formatting in the line frontend.
Neither services nor shared interaction models import `cli`, `tui`, `client`,
Ratatui or Crossterm.

### Feature presenters

Each feature owns its domain projection and valid feature transitions. For
example, selecting a session requests resume; selecting a task opens task
details; changing a status filter requests a new task query.

It supplies stable item/action/field identities, labels and descriptions,
structured content, validation feedback, available actions with disabled
reasons, action consequences, and a semantic interaction pattern. It declares
whether an action is a permission grant, a review decision, destructive, or
read-only. It does not supply colors, dimensions, key codes or footer hints.

Feature-specific workflow remains explicit Rust code. The shared host does not
learn the business rules of memory, approval, sandbox or sessions. Functional
transitions such as list-to-detail may be requested by a presenter; generic
back/focus/scroll behavior is owned by the interaction layer.

“Needs a choice, optional feedback and a submit action” is a valid feature
requirement. “Draw a centered 80% popup and toggle on j/k” is not.

### Shared interaction

Use a small typed set of patterns: choice, decision with optional feedback,
collection with filters and details, structured form, read-only document, and
composer with completion/history. Their state is reusable across features.
Containers compose these patterns, including a collection of reviews with a
decision control for the selected item. No arbitrary JSON widget tree or
runtime scripting language is required.

The host coordinates presenter state, reusable control state and effects.
Generic interaction state changes synchronously without I/O. It emits typed
effect requests; an async driver invokes local services or the remote client
and returns correlated results. Feature presenters receive semantic actions
and typed results, never raw terminal events or a backend handle.

The driver multiplexes pending effects with terminal input, streamed events and
redraws; it never waits for a query or write to finish inside input dispatch.
Writes still obey domain admission. Independent reads may proceed concurrently
with a running turn without permitting overlapping writes to the session.

The state used for rendering and event routing is the same surface/focus tree.
Underlying surfaces may remain visible but cannot accidentally consume input
captured by the active surface. Returning from detail or dismissing a surface
restores the prior focus, selection, scroll and composer draft.

### Terminal presentation

The terminal adapter maps physical input to semantic interaction events. It
resolves keys using the focused control and current bindings; a text field
consumes text before list shortcuts. Mouse and paste target that same focus
model. Rendering consumes an immutable snapshot and returns geometry used for
scroll limits, cursor placement and hit testing, without querying a service.

Shared controls own borders, spacing, selection, focus, empty/loading/error
states and action bars. A theme owns semantic styles. TUI composition decides
whether an interaction is an inline panel, overlay or full-size view based on
its pattern and available terminal space. Feature identity must not introduce
another isolated visual/navigation policy.

Keep the existing Markdown/transcript rendering, terminal restoration and
clipboard behavior where they are sound. Put them behind the new boundaries;
there is no requirement to rewrite working rendering algorithms.

## Interaction rules

1. Tab/BackTab move among focusable controls; arrows navigate the focused list,
   tree or choice. Enter activates the focused action/selection or submits the
   designated single-line input. Multiline composition retains an explicit,
   documented submit binding. Page keys scroll by the visible page size.
2. Esc goes back or dismisses the active surface. It does not approve, reject,
   withdraw or cancel a domain operation. Approval dismissal leaves the request
   pending and provides a visible route to reopen it. Reject is an explicit
   labeled action. At the root composer, Esc may request cancellation of the
   active turn; the host applies this only after focused surfaces consume Back.
3. Ctrl+C has one centrally implemented cancel/exit policy. Closing a view,
   cancelling a query, cancelling a turn and withdrawing an approval are
   distinct semantic actions. Preserve the existing double-press exit guard.
4. Permission decisions default to Deny. Memory/reflection reviews default to
   Accept, per the existing user decision. Opening or refreshing a view never
   submits either default. Consequences such as persistent grants are disclosed.
5. Primary interaction works without alphabetic shortcuts, including under a
   Chinese input method. Existing shortcuts may remain as aliases when they
   do not conflict with text input; displayed hints come from the bindings that
   actually handle the active control, not strings copied into feature titles.
6. Submission shows an in-flight state and blocks duplicate dispatch for that
   operation. Failure preserves input and exposes retry when safe. A decision
   accepted by the service and its domain application completing are displayed
   as distinct states. Unknown effects are reconciled, never blindly retried.
7. Item identity survives refresh/reordering. If the selected item disappears,
   select a defined neighbor; never apply its pending action to a replacement
   row at the same index. Empty collections have no activatable selection.
8. Action availability includes current backend capability and turn admission
   state. Busy turns continue streaming while users inspect data or type; the
   host uses the canonical admission/active-input contract to submit work.
9. Small terminals retain actions and focus visibility through scrolling or a
   compact layout. Read-only detail surfaces offer complete content rather
   than making truncation the only way to inspect an operation.

Changing approval Esc from Deny to Dismiss is an intentional UX compatibility
change. Document it and update behavioral/PTY expectations as part of migration.

## Effects, lifecycle and recovery

Every asynchronous result carries session/workspace identity, view instance and
query generation, plus the target item/operation identity needed for its use.
A late read result for a closed view or previous filter cannot replace current
data. Repeated refreshes are coalesced; local blocking storage reads run away
from the event/render path, and remote queries are awaited by the async driver.

Closing a view may cancel its read subscription. It does not cancel a domain
write that was already admitted. The driver still consumes that result and
updates authoritative projections or notices. Session switches detach view
subscriptions and invalidate old query results, while operation receipts remain
associated with their original session and target.

Reconnect reloads authoritative state before actions become available again.
Approval ids, versions and workspace/session scopes retain the existing service
validation; pending approvals are reconstructed from the durable service.
Transient focus, navigation and drafts are not an alternative approval journal.
UI validation and disabled controls never replace domain validation.

The same interaction host runs in local and attach mode. Backend adapters only
translate queries, commands and events. Capabilities are explicit; unavailable
remote functions have an explanation rather than a hidden local-only branch in
key handling. Add a missing endpoint only for a concrete current feature after
checking its domain and authorization contract. Preserve read-only reconnect,
turn identity, cancellation and terminal shutdown invariants.

## Existing feature migration

| Surface | Semantic interaction | Presenter-owned behavior |
| --- | --- | --- |
| Resume, archive, model, thinking | Choice plus labeled actions | Data, current choice, resume/archive/switch/apply/save semantics |
| Approval | Decision + document + optional feedback | Exact request, consequences, choices and human/application state |
| Memory/reflection review | Collection + detail + decision | Scoped candidate data, accept/reject and stale-candidate handling |
| Context | Tree/collection + document | Context sections/items and token estimates |
| Agents/tasks | Filtered collection + detail | Queries, status/workspace filters, task identity and transcript data |
| Sandbox | Form/choice + status/detail | Valid changes, preview, persistence and authorization requirements |
| Session/model/status reports | Structured detail | Facts and available operations; renderer chooses presentation |
| Memory, skills, MCP, reflection/timer reports | Collection/detail/form as applicable | Existing queries/actions, with text details for unstructured output |
| Help and slash completion | Command collection/completion | Command descriptions, argument suggestions and availability |
| Composer, transcript, running-task panel | Composer/document/status | Admitted user input and canonical streamed events |

Use one TUI command/action catalog for completion, help, feature entry and
availability. Typed slash arguments and interactive actions target the same
feature operations. ACP retains its negotiated command subset and HTTP retains
its domain APIs; a TUI catalog is not a new authority or a reason to expose all
commands to every transport.

## Code placement and dependencies

Proposed ownership under `crates/otto/src/tui/`:

- `features/`: presenters grouped by existing domain feature.
- `interaction/`: semantic events, reusable pattern state, focus/surface host.
- `widgets/`: Ratatui controls and shared visual behavior.
- `backend/`: local/attach adapters and async effect driver.
- `app.rs`: frontend composition and cached application projections.
- `render.rs`: shell composition over the active snapshot.
- `mod.rs` and `attach.rs`: terminal/connection startup and shutdown.

Exact file splitting follows the ownership above. Shared control state has no
Crossterm/Ratatui/backend imports; widgets never invoke services; presenters do
not implement raw key handling. The host connects the parts at the frontend
composition root. `app::Controller` and runtime services remain independent of
the frontend; extend the existing runtime boundary guard for new ownership.

Keep presentation models native/frontend-owned initially. Only genuinely shared
domain/wire contracts used by browser or protocol clients belong in `otto-core`.
The browser and chat clients may adopt the semantic patterns later using their
own controls; this proposal does not force terminal navigation onto them.

## Delivery and acceptance

1. Reconcile the approved `unified-human-input` changes and inventory every
   current entry point, local/attach capability and behavior to preserve.
2. Implement the host, input mapping, async effects and shared controls with
   choice and decision as the first complete vertical migrations. Migrate
   approvals and memory review together; remove their independent key paths.
3. Migrate Context and Agents to collection/tree/document patterns. Verify
   refresh identity, back navigation, details and nonblocking local/remote reads.
4. Migrate configuration, management reports, help/completion and composer.
   Route idle/busy and local/attach input through the same host. Remove old
   overlay flags, independent focus priorities and synchronous remote bridging.
5. Update the ownership map, detailed developer contracts, user manual and
   changelog; complete verification and open a PR. The migration is complete
   only when every current surface follows these boundaries. Intermediate
   adapters are temporary and must not become a second permanent framework.

Behavioral tests cover focus routing, nested Back, default choices without
automatic dispatch, text/paste targeting, generated hints, duplicate submission,
failed/unknown application, refresh/reordering, stale results and session switch.
Exercise identical semantic traces against local and fake remote backends.

Ratatui buffer checks cover consistent control states and narrow/short layouts.
Controlled PTY tests cover Chinese text, arrows/Enter, approval dismissal and
reopening, live output during queries, reconnect and raw-mode restoration.
Architecture guards reject feature key handlers, renderer/backend coupling and
runtime imports of frontends. Run focused checks then `make check-fast`; CI
remains responsible for the full platform gates.

## References

- [Code ownership](../code-organization.md)
- [Development contracts](../development.md)
- [Unified approval](2026-10-04-unified-approval.md)
- [Approval conversations](2026-10-03-agent-self-help-and-approval-control.md)
- [Context inspector](2026-09-25-context-inspector.md)
- [Agents view](2026-09-25-agents-view.md)
- [Native/frontend crate boundaries](2026-10-05-otto-crate-split.md)
