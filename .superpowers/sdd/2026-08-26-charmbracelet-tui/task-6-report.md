# Task 6 report

## Status
- Implemented streaming turn handling, bounded turn messages, batched streaming renders, tool/event/error handling, Enter submit, and Esc cancellation in `internal/tui`.
- Stage 1 scope preserved.

## TDD log
1. Added RED tests in `internal/tui/model_test.go`:
   - `TestPromptCommandStreamsEventsAndCompletes`
   - `TestToolEventsUpdateTranscript`
   - `TestDraftRemainsEditableWhileRunningAndEnterDoesNotQueue`
   - `TestEscapeCancelsActiveTurnAndWaitsForCompletion`
   - `TestPromptErrorLeavesModelUsable`
   - `TestFatalPersistenceErrorQuits`
   - `TestTurnChannelCancellationDoesNotLeakWorker`
2. Ran RED:
   - `go test ./internal/tui -run 'Test(Prompt|Tool|Draft|Escape|Fatal|TurnChannel|Usage)'`
   - Failed as expected with missing `turnMsg`, `renderTickActive`, and `fatalErr` behavior.
3. Implemented the minimal production changes in:
   - `internal/tui/messages.go`
   - `internal/tui/model.go`
4. Ran GREEN/fix cycle:
   - Focused test pass after implementation.
   - `go test -race ./internal/tui` exposed a cancellation edge where the final done envelope could be dropped; fixed by preferring a non-blocking done send before falling back to cancellation-aware send.
5. Re-ran GREEN and broader validation.

## Commands run
- `go test ./internal/tui -run 'Test(Prompt|Tool|Draft|Escape|Fatal|TurnChannel|Usage)'`
- `gofmt -w internal/tui`
- `go test ./internal/tui`
- `go test -race ./internal/tui`
- `go test ./...`
- `go test -race ./...`
- `git diff --check`

## Leak/cancellation evidence
- Turn channel capacity is asserted as exactly `64` in `TestPromptCommandStreamsEventsAndCompletes`.
- `TestTurnChannelCancellationDoesNotLeakWorker` repeatedly starts a streaming worker, stops consuming after the first event, sends `Esc`, and waits for the backend worker to exit. This exercises the load-bearing case where the bounded channel can fill unless cancellation-aware sends unblock correctly.
- `TestEscapeCancelsActiveTurnAndWaitsForCompletion` verifies `Esc` only calls the active cancel and the model stays running until the done envelope arrives.
- `go test -race ./internal/tui` and `go test -race ./...` both pass.

## Self-review
- `Update` remains the only mutator of `Model` state.
- Prompt execution is in a worker goroutine behind a bounded channel.
- Streaming text mutates raw assistant state immediately and defers markdown rendering behind at most one 50 ms tick, with immediate final render on completion/error.
- Tool start/result, usage, ordinary errors, and fatal persistence errors now follow `agent.Event` semantics.
- Editor drafting remains enabled while running; Enter does not queue a second prompt.
- Scroll/autofollow behavior was preserved by refreshing content with existing offset rules.

## Concerns
- The guaranteed key/command matrix beyond Enter submit and Esc cancel is still deferred to Task 7, per the task brief.

## Fix Round 1

### Fixes
- Invalidated the active assistant render cache on every text delta and forced completion/tool transitions to render the full accumulated `Raw` text.
- Flushed dirty assistant text before both tool-start and tool-result transitions; render ticks only clear dirty state after successfully rendering an active assistant.
- Kept the turn channel capacity at exactly 64 and reserved one slot for the real completion envelope with a 63-permit event semaphore. Cancellation-aware event sends cannot occupy the completion slot, so the worker can enqueue exactly one ordered completion and exit even if the UI stops consuming.
- Changed `EventAgentError` handling to record one visible error while keeping the turn running. The done envelope now exclusively completes/cancels lifecycle state and decides fatal quit behavior.
- Added turn-generation identity to render ticks so a delayed tick from an earlier turn cannot render or clear a later turn's state.
- Captured viewport Y offset before editor/viewport dimension and content updates, then restored it when auto-follow is disabled.

### Regression tests
- Added coverage for a real first render tick followed by a later delta and completion.
- Added text-before-tool-start and text-before-tool-result flush coverage.
- Added full-channel cancellation coverage asserting exactly one real completion error and worker exit.
- Strengthened ordinary and fatal agent-error tests to verify error-event-before-done lifecycle and no duplicate error entries.
- Added stale render-tick generation and exact viewport-offset preservation coverage.

### RED evidence
Ran before production changes:

```bash
go test ./internal/tui -run 'Test(StreamingRenderTickThenLaterDeltaCompletesWithFullRaw|ToolTransitionFlushesDirtyAssistantText|CanceledFullTurnChannelDeliversRealCompletion|PromptErrorLeavesModelUsable|FatalPersistenceErrorQuitsAfterCompletion|StaleRenderTickDoesNotMutateNextTurn|ViewportRefreshPreservesOffsetBeforeTemporaryClamp)$' -count=1
```

Result: `FAIL` (exit 1). The failures showed:
- completion remained rendered as `"first"` while `Raw` was `"first second"`;
- tool start left assistant text unrendered and dirty;
- the canceled full channel delivered zero completion envelopes and lost the backend error;
- `EventAgentError` made the model idle immediately instead of waiting for done;
- fatal error handling quit on the event instead of completion;
- a stale tick rendered turn B and cleared its dirty/tick state.

The viewport-offset test was included in the RED run and already passed against the current Bubbles implementation because its width/height setters do not presently clamp; the production capture was still moved ahead of all potentially clamping operations to enforce the invariant.

### GREEN evidence
After the minimal fixes:

```bash
go test ./internal/tui -run 'Test(StreamingRenderTickThenLaterDeltaCompletesWithFullRaw|ToolTransitionFlushesDirtyAssistantText|ToolResultTransitionFlushesDirtyAssistantText|CanceledFullTurnChannelDeliversRealCompletion|PromptErrorLeavesModelUsable|FatalPersistenceErrorQuitsAfterCompletion|StaleRenderTickDoesNotMutateNextTurn|ViewportRefreshPreservesOffsetBeforeTemporaryClamp)$' -count=10
go test ./internal/tui -count=1
go test -race ./internal/tui -count=1
go test ./... -count=1
go test -race ./... -count=1
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
test -z "$(gofmt -l .)"
git diff --check
go build -trimpath -o ./otto ./cmd/otto
```

Result: all commands passed.

### Fix Round 1 concerns
- Task 7 remains out of scope.
- The completion guarantee assumes `Backend.Prompt` eventually returns after context cancellation; Otto no longer adds a worker leak through blocked event/completion channel sends, but it cannot force a non-cooperative backend function to return.
