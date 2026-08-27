# Task 4 Report: provider-neutral transcript/history conversion and Markdown rendering

## Summary
Implemented provider-neutral transcript/history conversion in `internal/tui/entries.go` and Markdown rendering/fallback in `internal/tui/markdown.go`, with focused tests in `internal/tui/entries_test.go` and `internal/tui/markdown_test.go`.

## Files changed
- `go.mod`
- `go.sum`
- `internal/tui/entries.go`
- `internal/tui/entries_test.go`
- `internal/tui/markdown.go`
- `internal/tui/markdown_test.go`

## TDD evidence

### 1) Wrote tests first
Created failing tests before production code:
- `TestEntriesFromHistoryPairsToolResults`
- `TestEntriesFromHistoryHandlesZeroBlocksOrphansAndToolErrors`
- `TestEntriesFromHistoryPreservesMultipleTextBlocksAndStableIDs`
- `TestEntriesFromHistoryDefensivelyCopiesAndSaturatesUsageTotals`
- `TestMarkdownStripsOnlyTrailingLayoutNewline`
- `TestMarkdownFallsBackToSafePlainTextOnFailure`
- `TestMarkdownFallsBackWhenRendererIsNil`
- `TestMarkdownGlamourRendererRendersMarkdown`

### 2) RED
Command:
```bash
go test ./internal/tui -run 'Test(Entries|Markdown)'
```
Result:
```text
# github.com/baiyuqing/otto/internal/tui [github.com/baiyuqing/otto/internal/tui.test]
internal/tui/entries_test.go:20:20: undefined: EntriesFromHistory
internal/tui/entries_test.go:21:45: undefined: EntryTool
internal/tui/entries_test.go:37:16: undefined: EntriesFromHistory
internal/tui/entries_test.go:41:24: undefined: EntryAssistant
...
FAIL	github.com/baiyuqing/otto/internal/tui [build failed]
```
Expected failure confirmed: tests referenced the not-yet-implemented TUI transcript/rendering API.

### 3) Minimal implementation for GREEN
Added:
- pinned dependencies:
  - `charm.land/lipgloss/v2 v2.0.6`
  - `charm.land/glamour/v2 v2.0.1`
- `EntryKind`, `Entry`, `EntriesFromHistory`
- orphan-safe tool call/result pairing by call ID using a pending queue
- stable deterministic entry IDs
- defensive text/JSON conversion into entry fields
- saturating nonnegative usage accumulation
- `MarkdownRenderer` interface
- `GlamourRenderer`
- `renderMarkdown` fallback to escaped plain text plus nonfatal marker
- one-trailing-newline trim for successful Glamour output

### 4) Focused GREEN
Commands:
```bash
gofmt -w internal/tui
go test ./internal/tui -run 'Test(Entries|Markdown)'
```
Result:
```text
ok  	github.com/baiyuqing/otto/internal/tui	0.413s
```

### 5) Broader verification and one follow-up fix
First broader run:
```bash
go test ./...
```
Initially exposed a markdown fallback assertion mismatch and a regex-coverage issue for test names. I then:
- renamed markdown tests to `TestMarkdown...` so the focused command actually exercises them
- corrected the fallback assertion to match the intended single-backslash escaped plain text output

Re-run results:
```bash
go test ./...
```
```text
ok  	github.com/baiyuqing/otto/internal/tui	0.189s
ok  	github.com/baiyuqing/otto/internal/... (all packages passed)
```

## Final validation
Commands run successfully:
```bash
go test ./internal/tui -run 'Test(Entries|Markdown)'
go test ./...
go test -race ./...
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
test -z "$(gofmt -l .)"
git diff --check
```

## Behavior delivered
- Provider-neutral transcript conversion from `[]model.Message`
- Stable transcript entries for user/assistant/tool/system kinds
- Pairing of tool results back onto prior tool calls by `ToolCallID`
- Orphan-safe preservation of unmatched tool result data
- Empty assistant responses preserved as empty assistant entries
- Multiple text blocks merged when contiguous and split around tool blocks to preserve order
- Defensive handling of malformed/unknown-but-loaded tool-like blocks
- Usage totals that never go negative and saturate safely on overflow
- Glamour Markdown rendering with adaptive dark/light style selection from Lip Gloss background detection
- Safe plain-text fallback when Markdown rendering fails or no renderer is available
- Rendering errors remain nonfatal to display

## Self-review
- `internal/tui` stays provider-neutral and offline: it only consumes `internal/model` types and local rendering libraries.
- No Bubble Tea model/layout/streaming work was added.
- Tool result pairing is intentionally queue-based for duplicate call IDs, which is safer than a single-entry map for malformed histories.
- Fallback rendering escapes control characters instead of returning raw terminal sequences.

## Commit
- `feat: add TUI transcript rendering`

## Fix Round 1

### Findings
- Preserved zero-block tool transcript entries instead of dropping them.
- Mapped `model.Role("error")` to `EntryError` while keeping unknown-role fallback on `EntrySystem`.

### RED
Command:
```bash
go test ./internal/tui -run 'Test(Entries|Markdown)'
```
Result:
```text
--- FAIL: TestEntriesFromHistoryPreservesZeroBlockToolMessages (0.00s)
    entries_test.go:36: len(entries) = 0, want 1 ([]tui.Entry{})
--- FAIL: TestEntriesFromHistoryMapsErrorRoleToEntryError (0.00s)
    entries_test.go:54: entries[0] = tui.Entry{ID:"message-0-role-error-text-0", Kind:"system", Raw:"", Rendered:"", RenderWidth:0, ToolCallID:"", ToolName:"", ToolArgs:"", ToolOutput:"", ToolError:false, ToolDone:false}, want error entry
FAIL
```

### GREEN
Command:
```bash
gofmt -w internal/tui
go test ./internal/tui
go test ./...
```
Result:
```text
ok  github.com/baiyuqing/otto/internal/tui
ok  github.com/baiyuqing/otto/... (all packages passed)
```
