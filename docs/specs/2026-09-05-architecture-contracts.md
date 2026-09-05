# Architecture and core-data contract cleanup

Status: implemented and verified on 2026-09-05 after user approval.
Base: `148b603`; branch: `codex/architecture-contracts`.
The worktree was fast-forwarded after main incorporated documentation and
security changes (#67/#68) during this design review.

## Objective and boundaries

Address the two architecture/data-model reviews by giving shared data one set of
invariants, keeping mutable task state behind the application boundary, and
making lifecycle ownership explicit. Keep the existing package layout and the
two supported providers. Use current dependencies and direct Go composition.

Preserve existing CLI behavior, HTTP field meanings, append-only Pi v3 history,
redaction, workspace confinement, sandbox defaults, pending memory review, and
child-agent tool restrictions. Old history must remain readable. Do not rewrite
existing session files or make child transcripts persistent.
Preserve the newly landed os.Root workspace lifetime, confined definition reads,
sandboxed startup command execution, and metadata-only HTTP tracing.

The parent owns design, work sequencing, integration review, and acceptance.
Subagents implement bounded, explicitly owned changes after approval using TDD.
Changes stay in this worktree; existing worktrees and root guidance are preserved.

## Decisions

### 1. Shared message invariants and copying

- Keep `model.Message` and the tagged `model.Block` representation. Do not add
  an interface hierarchy for message/block variants.
- Add one neutral message validator for role/block compatibility, required tool
  identity, complete tool arguments, conflicting fields, and usage validity.
  Allow legitimate transient messages whose ID/timestamp is assigned later.
- Keep Pi encoding rules, size limits, ID generation, reserved checkpoint
  handling, and historical record validation in `session`.
- Both Session implementations enforce the same conversation sequencing rules,
  on every Append, including tool-call/result pairing. A shared helper owns those
  rules; it must not round-trip transient messages through Pi to validate them.
  Preserve legitimate empty assistant blocks and RoleContext usage in memory.
- Put deep-copy operations for Message/Block/Usage in `model`; replace the
  duplicated implementations in agent, app, and session. All reference fields,
  including newly introduced metadata, must be copied at ownership boundaries.

Acceptance: a malformed message/sequence is rejected by both session backends;
mutating an input or returned history does not change stored state; existing
valid transient and persisted messages continue to work.

### 2. Structured context metadata with history compatibility

- Add narrow optional, typed context metadata for task identity. Retain the
  existing `ContextType`, `ContextTokensBefore`, and `Display` fields to avoid
  an unrelated HTTP history schema migration.
- Deliver notifications with TaskID in both the event and the Message metadata.
  Encode it as `details: {"otto": {"taskId": "t1"}}` on Pi custom-message
  records. The local codec already preserves optional raw `details`; compatibility
  claims are limited to the checked-in codec/fixtures until an interop check runs.
- Decode typed metadata first. Recover TaskID from the legacy notification text
  only when reading old records that lack the structured field. New records and
  normal rendering must not depend on wording.
- Keep metadata bounded and validate its shape. TaskID accepts only generated
  structural IDs (`t` plus bounded decimal digits), matching the existing live
  TaskID event contract; do not independently transform the identity. Notification
  text still uses normal redaction. Do not accept arbitrary metadata content.

Acceptance: new notification metadata round-trips; existing fixtures still load;
changing a notification header does not lose its task association; live and
resumed rendering agree. A fixture-level check verifies Pi compatibility.

### 3. Provider response, events, and tool-result semantics

- Make `Response.Message.FinishReason` and `Response.Message.Usage` the single
  source of response metadata; remove the duplicate fields on Response.
  Retain the Response envelope to minimize provider API churn.
- Preserve usage presence from provider decoding: `nil` means unavailable;
  a non-nil zero Usage means an explicitly reported zero. Normal turns and
  compaction consume the same representation and validation rules. Presence must
  survive session accumulation/reopen; negative counts or cached > input are
  invalid regardless of presence.
- Pi requires an assistant usage object. For newly written explicit-zero usage,
  add `details: {"otto": {"usagePresent": true}}` using its existing optional
  details field. Legacy zero objects without the marker retain nil normalization.
  Parse only the exact namespaced boolean, tolerating absent/unrelated details.
  This records information in new records without inventing it for old ones.
- Add an event usage-presence flag so missing usage is not reinterpreted as known
  zero. Preserve existing HTTP usage object keys; any presence flag is additive
  and optional. Aggregation and rendering respect the presence bit.
- Reuse one compaction payload for direct results and agent events. Keep
  HTTP/SSE wire DTOs separate because they have their own compatibility contract.
- Replace the `PersistedContent == ""` sentinel with an optional string:
  nil selects Content, a pointer selects its value including an empty string.
  Preserve the current-turn tool-result overlay and redaction behavior.
- Document that Provider and Tool instances may be shared by parent and child
  agents; calls must be concurrency-safe, request data is borrowed read-only,
  and per-call emit callbacks are ordered and finished before the call returns.
  Consumers must not mutate event payloads. Keep streaming fragments as strings;
  a partial argument fragment is not necessarily valid JSON.

Acceptance: missing/zero/invalid usage has consistent behavior in both adapters
and both execution paths; empty persisted override works; concurrent calls do
not share per-call response state; existing wire payloads remain compatible.

### 4. Task mutation and frontend capability boundary

- Keep Task as an immutable-by-convention query snapshot and taskEntry as private
  runtime state. Replace unrestricted `Update(func(*Task))` with operations for
  starting, progress recording, and completion.
- Identity and creation fields never change. Permit queued -> running and
  queued/running -> terminal transitions; reject terminal regression and repeated
  completion. Cancellation requests do not report completion until execution
  has actually stopped. Completion notification must exist before Wait returns.
- Keep `app.TaskLister` as the optional frontend capability entry point, but
  return a narrow task view/control interface rather than `*agent.Tasks`.
  Frontends can list/get/history/cancel/wait/observe changes, but cannot access
  Add, state mutation, Close, or the raw Inbox.
- Controller privately accesses the runner through an owner interface returning
  `*agent.Tasks`; this is separate from TaskLister because Go method return types
  are not covariant. Keep the frontend view stable for one session generation.
- Add PrepareWake(ctx), which atomically checks pending notification work and
  claims a turn through the same admission rule as Prompt/Compact. It returns
  no operation when there is no work. Return a concrete one-shot WakeOperation
  reusing the existing active-operation state, with Run and idempotent Cancel.
  Frontends publish their turn only after successful admission, then execute it.
- An unstarted claim must be released on cancellation or Controller.Close, and
  Run after release must fail without invoking the agent. Callers release the
  claim if frontend scheduling/publication fails. Test this ownership explicitly;
  an abandoned claim must never leave Close waiting forever.
  Keep frontend event loops as triggers; add no autonomous scheduler. TUI keeps
  modal/editor restrictions. One-shot execution still waits for children and
  drains their final notifications before exiting.
- Define update-channel ownership explicitly and preserve session replacement
  behavior. Updates remains a coalescing single-consumer edge signal for the
  selected frontend, not a broadcast subscription. No controller goroutine reads
  it. A wake that claims no work must not publish a new HTTP-visible turn.

Acceptance: terminal transitions and identity cannot be overwritten; duplicate
wake triggers do not produce duplicate turns; task completion at turn finish
is not missed; an unstarted claim cannot block shutdown; cancellation, task wait,
TUI gating, and one-shot draining work.

### 5. Explicit Controller shutdown and construction

- Remove goroutine identity and callback-depth detection. Callbacks may request
  closure, but may not synchronously wait for their own operation to finish.
- Add `RequestClose()` as an idempotent nonblocking request. It only marks state,
  rejects future admission, and delegates cleanup to an active/replacement owner;
  it never invokes a closer. Preserve `Close() error` as the synchronous operation
  that requests closure and claims idle cleanup or waits for its existing owner.
  Concurrent Close calls share one completion/error. Do not add WaitClosed.
  Existing callers cancel their context before waiting, as they do today.
- No new background cleanup loop. Active operations/replacements retain cleanup
  ownership until completion; the external lifecycle owner handles idle cleanup.
  Close each current/candidate session and runner exactly once, including errors.
- Construct `app.New` from a prebuilt `SessionReplacement`. Remove placeholder
  create/build factories and the unused fallback construction path. A new-session
  operation requires the configured NewSessionBuilder.

Approved internal API change: callback code using synchronous Close must use
RequestClose. A standalone idle RequestClose requires the external owner to call
Close to complete cleanup. This is currently exercised by controller tests;
production shutdown callers operate outside callbacks. CLI/HTTP shutdown still
waits for required cleanup and reports errors.

Acceptance: request from an emit/build/cleanup callback cannot deadlock; external
shutdown waits for blocked cleanup and returns its errors; no goroutine-stack
parsing remains; startup/new/resume/profile/server share the same construction.

### 6. Authentication and shared profile selection

- Expose a narrow authentication capability through app. Wire credential paths
  and concrete auth operations at the composition root; frontends never receive
  Credentials or manipulate credential files.
- Keep OAuth and credential persistence in auth. Frontends own browser/link
  presentation, synchronous versus asynchronous UI work, and output formatting.
- Preserve the immutable startup credential snapshot and the existing restart
  requirement after interactive login. Preserve fixed/redacted auth errors.
- Put profile selection followed by default-profile persistence in one app
  operation. Preserve the partial-success contract: failure saving the default
  does not silently undo an already completed session replacement. Both frontends
  refresh from the resulting backend state and report the persistence failure.

Acceptance: both frontends exercise the same credential and profile operations;
unsupported/unavailable login and partial profile success are handled uniformly;
OAuth tests stay offline; shutdown/session replacement behavior is unchanged.

### 7. Single-source tool assembly

- Reuse one helper for built-in file-tool construction in runtime assembly and
  static boundary inspection. Export/reuse the bash definition from tool instead
  of copying its schema in cmd.
- Compute enabled tool groups consistently without invoking runtime construction
  recursively from the safety gate. Preserve conservative preflight checks and
  the final agent.New validation of the actual registry/system prompt.
- Test definition/gating parity for sandbox availability, skills, memory, and
  agent configuration. Preserve restricted child tool sets and secret handling.

The config dependency on endpoint-specific URL validation is retained: config is
an outer configuration adapter, and moving one validator into a new package adds
little isolation. Session backend replacement and generic file-tool effects were
extension-cost observations, not requirements to add new backend/plugin machinery.
Their current limitations remain explicit; they do not block these changes.

## Execution and ownership

| Work package | Owner/model | Files and dependencies |
| --- | --- | --- |
| A: message validation/copying | model worker, gpt-5.6-luna | model + session; then mechanical agent/app clone migration |
| B: task state and app task capability | task worker, gpt-5.6-luna | agent/tasks + subagent + app task facade; after A |
| C: shutdown and prebuilt construction | gpt-5.6-terra, finished by gpt-5.6-sol | app/controller + cmd runtime + server lifecycle; serialize with B |
| D: context metadata and response semantics | model worker, gpt-5.6-luna | model/session/provider/agent + focused frontend mappings; after B/C |
| E: frontend use cases and assembly | reused worker, gpt-5.6-luna | auth/app/frontends/cmd/tool definitions; after C/D |

Luna handled bounded model, provider, task, frontend, and assembly changes.
Terra performed the lifecycle design/initial implementation; Sol completed its
resource-ownership and test migration. The parent decided interfaces, reviewed
integration, and ran final acceptance checks. File ownership was handed off
between workers; independent reads/reviews ran concurrently.

## Verification and delivery

Every behavioral change starts with the smallest relevant failing test, then the
minimal production change, then its package gates. Pure cleanup must preserve the
existing behavior tests; a focused ownership/copy test covers any changed contract.
Workers report the RED command/failure and GREEN result, plus intentional API changes.

Final gates: `go build -trimpath -o ./otto ./cmd/otto`, `go test ./...`,
`go test -race ./...`, the offline PTY lifecycle test, `go vet ./...`,
`go run honnef.co/go/tools/cmd/staticcheck@latest ./...`, gofmt, and
`git diff --check`. Run relevant session interop script tests without fetching
new dependencies. Review both diff and untracked files. Publish or merge only
when explicitly requested.

Baseline at `148b603`: `go test ./...` passed with host permissions. The earlier
run at `51db08b` passed all packages except Seatbelt inside the enclosing restricted
sandbox (`self-test-failed`); that package passed with host permissions. This is
an execution environment difference, not a reason to bypass sandbox protection.

## Final verification

Passed on the completed worktree:

- `go build -trimpath -o ./otto ./cmd/otto`
- `go test ./... -count=1`
- `go test -race ./... -count=1`
- `go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=1`
- `go vet ./...`
- `go run honnef.co/go/tools/cmd/staticcheck@latest ./...`
- gofmt and `git diff --check`
- `node --test scripts/pi-session-interop.test.mjs` (three offline probe tests;
  these do not assert compatibility with an externally installed Pi release)

Focused regressions cover shared message validation/copying, metadata and usage
round-trips, terminal task-state protection, unknown child usage, abandoned wake
claims, callback close requests, idle closed-session snapshots, TUI wake Escape
cancellation/compaction accounting, one-shot wake errors, canceled authentication,
and profile replacement with failed default persistence.

An earlier full ordinary test run failed in the unchanged nativeprocess test
`TestManagerCloseTerminatesActiveGroupsAndRejectsNewRuns` with
`sandbox child termination failed`. Focused repetition failed 7/20 times in this
worktree and 2/20 times in the original main checkout. The nativeprocess package
and its sandbox dependencies have no diff. The final isolated ordinary suite and
full race suite passed; the existing intermittent nativeprocess failure remains
unfixed. Its syscall-level cause was not established, and no termination/security
check was weakened to obtain a pass.
