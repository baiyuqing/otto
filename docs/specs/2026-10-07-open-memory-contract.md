# Open Memory service contract

Status: Rust contracts, host admission, built-in projection and runtime injection
implemented on 2026-10-07. External transports and a semantic-search upgrade are
not included. The [research records](2026-10-07-open-memory-research/README.md)
preserve the source evidence and its limits.

## Objective and decisions

Otto expresses memory needs to a service. The service owns how it forms,
retrieves, evolves and stores memory. Replacing that implementation must not
require changing the agent, model tools or frontend use cases.

Decisions confirmed by the user on 2026-10-07:

- Design the service contract first; the interface is not limited to the
  initial examples `recall`, `remember`, `reflection` and `forget`.
- Rust extensions may adapt independent services such as Mem0. Do not require
  an external service to implement Otto's repository schema or algorithms.
- Deliver behavior contracts and Rust interfaces first; adapters call native
  remote APIs without requiring a new standard HTTP protocol.
- Support reviewed changes and explicitly delegated service learning. Preserve
  the current review policy as the default.
- Retain research records separately from the resulting design.

The earlier proposed embedding/FTS upgrade belongs to the built-in
implementation's follow-up, not to this service contract. No vector database,
graph representation, extractor, ranker or tokenization algorithm is required.

## Capability surface

`MemoryService` has two required operations, `describe` and `recall`. Other
operations have default `Unsupported` results and are usable only when their
capability is declared. Capability absence is distinct from an unavailable
service and from an unverified deployment guarantee.

| Operation | Input intent | Result | Required |
| --- | --- | --- | --- |
| `describe` | Read the bound service's contract | Identity, contract version, capabilities and guarantees | Yes |
| `recall` | Obtain context for the current task | Consumable items, references, provenance and coverage | Yes |
| `observe` | Submit interaction or task evidence | Receipt; memory formation is not guaranteed | No |
| `remember` | Express information to create or correct | Receipt, or a prepared change set through `prepare` | No |
| `reflect` | Process an explicit body of evidence or source range | Receipt for extraction, reflection or consolidation | No |
| `forget` | Forget identified items, sources or an authorized scope | Receipt and actual coverage | No |
| `inspect` | Get an item, list items or read service state | A bounded management projection | No |
| `feedback` | Report usefulness, incorrectness or obsolescence | Receipt describing the signal's effects | No |
| `prepare` | Prepare a supported mutating request for review | Reviewable changes and a bound preparation reference | No |
| `apply` | Apply an approved preparation | Execution receipt | No |
| `operation_status` | Follow a service operation reference | Updated receipt | No |

These are service operations, not necessarily model-callable tools. The
existing `remember`, `forget`, `memory_search` and `reflect` tools remain Otto
facade operations; their presence and behavior follow authorization and actual
capabilities. A capability declaration never makes a new model tool appear.

`observe`, `remember` and `reflect` have different meanings:

- `observe` submits evidence, including messages, tool outcomes or completed
  task events. A service may archive it, learn from it, or support only one of
  those effects. An archive-only request cannot silently trigger learning.
- `remember` submits an explicit content or correction intent. It is not a
  promise that only one existing item changes: an implementation may reconcile
  related items if that effect is authorized and declared.
- `reflect` requests processing of supplied evidence or a declared source
  range. It need not depend on previous `observe` calls. A service whose
  consolidation is wholly automatic need not expose this operation.

History, change feeds, import/export and shared attachment management are
possible extensions, not version-one requirements. New common operations need
a real consumer and a versioned addition to the contract. Provider-specific
features stay in their adapter; there is no unrestricted model-visible
`invoke(anything)` escape hatch.

## Boundary values and Rust ownership

The intended neutral port lives in `otto-core`; native construction,
credentials, transports and persistence remain in `otto`. The existing
`agent::memory::MemoryRecall` stays as the agent's narrow read port, adapted
from the selected service. Native `memory::Service` remains the facade used by
tools and frontends. It no longer defines every service through `MemoryStore`.

The public Rust surface consists of `MemoryService`, `ServiceDescription`,
`MemoryContext`, the operation-specific requests/results and typed errors.
Operation methods are asynchronous and use the existing `OperationControl`;
the port follows the core's existing native/WASM async convention. Factories
and the startup composition inject the same service into every native entry
point and resumed session. Extend `embed` by addition only.

The implementation is `otto_core::memory`, the native
`memory::client::Client`, and `memory::adapter::BuiltinAdapter`. Inject an
independent service with `embed::OttoBuilder::memory_service(MemoryExtension)`
or `cli::run::run_with_extensions`; existing entry points retain their defaults.
`MemoryExtension::new` borrows the provider; `owned` transfers shutdown
responsibility to one host. `with_learning` requires an explicit host grant.
Memory disablement, a closed content boundary and the existing encryption
requirement still apply to injected services.

| Value | Contract |
| --- | --- |
| Service identity | Stable logical instance identity and contract version; no credentials or transport addresses |
| Capability | Supported operation and effects; available modes, target forms, preconditions, retry and completion guarantees |
| Memory context | Host-established principal, authorized scopes, workspace/session/task identity and caller origin |
| Observation | Stable event ID, content, source reference, role/origin, occurrence time and host trust classification |
| Memory reference | Service-owned opaque ID, bound to the service instance and scope; revision is optional and opaque |
| Recall item | Consumable content plus optional item/source references, effective time and service metadata |
| Prepared changes | Reviewable semantic changes, target preconditions, declared effects and an opaque preparation reference |
| Receipt | Admission, completion, visibility, per-item results, coverage and optional operation reference |

Fields are bounded owned values. Shared content is text or a typed resource
reference; an adapter may project a file, graph fact or other representation
into that form. A resource reference is not permission to fetch or execute it.
Otto does not serialize service addresses or credentials into references.

Requests are constructed from admitted Otto callers, not model-supplied
identity or authorization fields. Scope translation to a provider's user,
project, group or namespace is an adapter responsibility. Provider namespace
filters alone do not establish authorization.

Optional scores and confidence values retain provider-specific meaning; they
are not made comparable across services and never imply factual correctness
or human approval. Missing revision, source or time information is represented
as missing, not synthesized into a stronger guarantee.

Otto validates returned scope and content bounds, admits context within the
actual prompt budget, and retains authority over user-approved instructions.
External memory is reference material by default. Authorizing an input for
learning does not approve every derived item as a standing instruction.

## Authorization and effects

The service description declares whether each operation stores evidence,
changes semantic memory, or authorizes continuing background evolution.
Otto checks these effects before submitting a request.

### ReviewedChanges

This is the default and preserves current memory review behavior. Model and
extractor requests cannot directly authorize final memory mutations.

A supported path either prepares reviewable final changes for `apply`, or
guarantees that an approved direct operation performs only the specified
changes. Related semantic modifications must be included in those changes;
index/representation maintenance may follow only the declared allowed effects.
Source scope, targets, preconditions and effects are bound to the approval.
Changed or expired preparations require a new review.

The service must also satisfy the requested constraint on continuing
autonomous semantic changes in that scope. A candidate preview does not by
itself establish this. If the necessary guarantees are missing or unknown,
reject the requested use or expose an explicitly restricted capability set.
Never switch to delegated learning automatically.

### DelegatedLearning

An explicit user-owned grant authorizes the service to learn from admitted
inputs within named scopes. The grant specifies input classes and whether it
includes ongoing consolidation, merging or synthesis. Service learning is
governed by this grant rather than pretending each final item was reviewed.

Changing Otto back to ReviewedChanges stops future Otto admissions under the
grant; it does not prove a remote service stopped already accepted or ongoing
processing. Reconcile the remote guarantees before admitting strict writes in
that scope. Read-only use remains possible when permitted.

Human approval remains an Otto authority decision. A service-returned field
cannot grant rights, approve a candidate or authorize execution of recalled
procedures. Linked Rust extensions are trusted host code, not sandboxed code.

## Completion, retry and lifecycle

Admission, completion and retrieval visibility are separate facts:

- Admission: acknowledged, rejected or unknown.
- Completion: pending, running, complete, partial, failed or unknown.
- Visibility: ready, pending, unknown or not applicable.
- Per-item effects: applied, no change, skipped, failed or unknown, with safe
  reason categories and references where available.

An acknowledged request without a tracking handle can have unknown completion.
A batch job's success does not erase skipped or failed item outcomes. A service
may complete an operation before its output is searchable; it must not claim
retrieval visibility merely because submission succeeded.

Idempotency, conditional updates and status lookup are individually declared
guarantees. Caller-generated request/event IDs provide correlation; they do
not turn a non-idempotent provider operation into an idempotent one. A timeout
or cancellation after submission may leave an unknown outcome. Do not replay
an effectful request until its outcome or safe retry guarantee is established.
Reuse Otto's existing approval/application recovery semantics.

Errors distinguish invalid input, unauthorized scope, unsupported capability,
precondition conflict, rejected request, unavailability, rate limit and unknown
outcome. Public errors and receipts contain safe categories, not raw remote
payloads or secret-bearing transport errors.

Per-turn recall remains best effort: warn and continue without memory when it
fails. Effectful commands report their actual receipt/error; they never report
a pending or unknown write as completed. Closing a session releases its local
binding, not its memories. Service shutdown releases owned resources and
preserves references to accepted work whose completion is still pending or
unknown; it does not promise to cancel remote background processing.

The host client keeps up to 64 reviewed preparations. An apply atomically
claims one attempt before calling the service; pending, partial, unknown and
failed outcomes do not make that preparation replayable. A trusted host can
persist a snapshot and its digest, then restore it with authenticated human
context and `PreparationRecovery`. Recovery defaults to `Attempted`;
`NotStarted` requires durable host evidence that no attempt occurred. A digest
checks equality, not human approval. Service idempotency claims do not silently
enable retries. Borrowed-provider close releases only the client, while an
owned-provider close reports its actual shutdown result.

## Forgetting

`forget` names a target form (item, source or scope) and required coverage.
Coverage distinguishes retrieval suppression, semantic invalidation, active
data deletion and removal of identified derived content. Expiration, hiding,
detaching and supersession are not interchangeable with deletion.

The adapter reports actual coverage and remaining limitations. A service
unable to meet the requested guarantee returns an unsupported/precondition
result before effects when this is known, or a partial result if some effects
already occurred. Deleting a source does not automatically mean shared
summaries, graph state, profiles, backups or model parameters were erased.
Otto must not infer stronger guarantees from a successful HTTP response.

## Real-consumer mapping and acceptance

| Consumer | Natural mapping | Guarantee that must not be invented |
| --- | --- | --- |
| Current Otto | Recall; explicit writes; candidate preparation/review; inspection | Existing service/repository APIs are not yet this port |
| Mem0 Platform V3 | Learning ingestion; direct content intent; retrieval; events | Input approval is not final-change approval; Dream is ongoing |
| Zep/Graphiti | Episodes; graph queries; explicit graph changes; batch processing | Source deletion does not rebuild every shared summary |
| Letta | Standing context plus on-demand file/archival access | Current SDK and legacy blocks are different contracts |
| LangGraph/LangMem | Store inspection/direct writes; extraction; reflection execution | A submission Future does not prove background completion |

The acceptance suite uses offline fakes and the built-in adapter, with no
credentials or live provider calls. It covers:

- A recall-only service works; unsupported writes make no service call.
- Scope admission and prompt trust are enforced independently of service data.
- Observe, explicit content and reflection keep distinct effects and inputs.
- Strict review rejects changed targets, expanded effects and unknown ongoing
  learning; delegated mode needs an explicit grant.
- Duplicate events and writes respect the actual idempotency guarantee.
- Batches preserve partial/skipped results; job completion and visibility stay
  distinct; unknown outcomes are not automatically replayed.
- Forgetting distinguishes logical and physical effects and reports derived
  residuals; recall never elevates a learned item into a user-approved rule.
- Close/resume preserve accepted-operation state and existing append-only
  history. CLI, TUI, ACP, HTTP and embed use the same selected service.

## Current integration limits

The default agent recall and tools keep the native path, including the standing
set's independent limit/budget and existing review UI. The built-in Rust
projection declares only implemented capabilities; service-neutral `observe`,
`reflect`, feedback and jobs are not wired to the existing reflection subsystem.

Injected services use the neutral port for agent recall. Their context is
always reference material; metadata cannot promote a standing rule. An external
service in ReviewedChanges mode exposes read tools. Prepared-change review/apply
is available to its trusted Rust host; no native candidates or frontend review
flow are fabricated. In explicitly delegated mode, supported `remember` and
retrieval-suppression `forget` tools return actual service receipts.

Legacy record-management commands cannot represent arbitrary service objects
and return Unsupported for an injected service. The Rust session's
`memory_service()` and `memory_context(origin)` expose neutral inspection and
management. History/change feeds/import-export and external frontend review
presentation remain separate extensions. No Mem0 transport or new network
protocol is advertised as implemented.
