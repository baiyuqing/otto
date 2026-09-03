# Inline transcript design

Status: approved 2026-09-03. Branch `feat/inline-transcript`, worktree
`/Users/baiyuqing/Work/code/otto-inline-tui`.

## Problem

The TUI currently enables mouse reporting (`view.MouseMode = tea.MouseModeCellMotion`
in `internal/tui/model.go:640`) so that wheel scrolling can navigate the transcript
viewport. With mouse reporting active, the terminal intercepts drag events instead of
selecting text natively; users must hold Shift to select.

The alternate screen buffer (`view.AltScreen = true` in `model.go:639`) has no
scrollback history. Even if mouse reporting is disabled, only the currently visible
viewport can be selected, and the wheel cannot scroll back to earlier turns.

The result: text selection and scrolling workflows differ from the terminal's native
behavior and require workarounds (Shift+drag for selection).

## Goal

Align with Claude Code and Gemini CLI by moving transcript rendering to the
terminal's native scrollback and disabling mouse reporting.

- No alternate screen buffer, no mouse reporting.
- Finished transcript entries print to native terminal scrollback via
  `tea.Println`; text selection and scrolling are handled entirely by the
  terminal.
- Only the in-progress turn (streaming assistant text, running tool calls),
  slash-command suggestions, editor, and footer occupy the bottom of the screen
  as a live region; its height adjusts with content.

Non-goals: activating `PgUp`/`PgDn` within the live region, clearing scrollback
on `/new`, re-rendering already-printed lines on resize or `ctrl+o`, and
TypeScript CLI frontend.

## Design

### 1. Rendering mode

`newRootView` (in `internal/tui/model.go:637`) sets `AltScreen = false` and
`MouseMode = tea.MouseModeNone`. Keyboard enhancements and cursor positioning
remain unchanged.

### 2. Transcript structure: committed (scrollback) + live (viewport)

Existing `m.entries` remains the complete transcript; turn indices do not change.
Current turn logic (`turnEntryStart`, `reconcilePersistedToolResults`,
`finishToolEntry`, etc.) is untouched.

New state:

- `committed int`: `entries[:committed]` have been written to scrollback;
  `committedAssistantTurn bool` tracks grouping state across the commit boundary
  ("Otto" heading).
- Refactor `transcriptContent` (in `model.go:1572`) into a pure function
  `renderTranscript(entries []Entry, assistantTurn bool, width int) (string,
  bool)` shared by both commit and live rendering. Reuse the same block renderers:
  `renderUserBlock`, `renderToolBlock`, `renderMessageBlock`, `indentToolBlock`.
- Finalization rules (committable): User/System/Error always; Tool when
  `ToolDone`; Assistant when `index != activeAssistant`; Compaction when
  `!running` (since `updateCompactionEntryTokens` updates `TokensAfter` on
  result arrival). `commitFinalEntries()` advances `committed` to the first
  uncommitted entry.
- Commit points: initialization (backend history + logo/hint banner, banner no
  longer enters live region), after user message (`model.go:886`),
  `EventToolCallStarted`/`EventToolCallFinished`, `finishTurn`, non-turn appends
  (`login.go:177`, `memory.go:180/185`, `recordTurnError`), and all paths that
  reset `m.entries` (first set `committed = 0`, then flush all).
- `/resume` therefore prints the selected session history into scrollback.
- When a committed Tool entry is later rewritten by `reconcilePersistedToolResults`,
  scrollback keeps the event version; no corrective re-print is issued.

### 3. Print order

Bubble Tea executes each Cmd in its own goroutine and calls `p.Send`
(in `tea.go:729-740`); consecutive Updates returning `tea.Println` do not
guarantee order. Use: model holds `pendingPrints []string` and `printInFlight
bool`; emit `tea.Sequence(tea.Println(chunk), func() tea.Msg { return
commitFlushedMsg{} })` only when not in-flight; on `commitFlushedMsg`, send the
next chunk. Each chunk is joined with `"\n\n"` between chunks; a blank line
trails each chunk to separate it from later output.

### 4. Dynamic layout

`calculateLayout` (in `layout.go:46`) takes a live-region line count parameter:
`transcriptHeight = min(liveLines, availableHeight)`; when idle, zero. Total view
height equals live region + suggestions + editor + footer. The `tooSmall` check
(`width < 40 or height < 8`) is unchanged.

The live region still uses `viewport` (soft-wrap + clipping), always positioned at
the bottom. Remove: `autoFollow`, `syncAutoFollow`, `scrollViewportMsg`,
`MouseWheelEnabled`, and routing for `PageUp`/`PageDown`/`Home`/`End` to
viewport (Home/End now only affect the editor; Resume keys in picker remain).
Add a `ponytail:` comment at `commitFinalEntries`: in-progress entries exceeding
terminal height show only their tail; add live-region scroll keys if scrolling
becomes necessary.

Behavior of `ctrl+o`: toggles only uncommitted and later entries; already-printed
lines do not re-render. Window resize redraws only the live region; the terminal
handles reflowing scrollback.

Picker and overlay continue to use full `m.height` rendering; on close, the live
region shrinks. Trailing blank lines after exit are a known issue recorded here
and not handled in this iteration.

### 5. Exit

The inline view remains on screen after exit, consistent with Claude Code.

## Known limitations

- `ctrl+o` does not affect already-printed entries.
- `/new` does not clear scrollback.
- Window resize does not re-render scrollback; the terminal does that automatically.
- The live region may leave trailing blank lines when exiting or closing an overlay.
- In-progress entries exceeding terminal height show only their tail; no keyboard
  scroll keys are implemented for the live region yet.
