# Inline viewport for the Rust TUI

Status: implemented 2026-09-20. Branch `feat/tui-inline`, worktree
`.worktree/tui-inline`. Supersedes the mouse policy added in #136 and revisits
the Go-era design in
[2026-09-03-inline-transcript-design.md](2026-09-03-inline-transcript-design.md),
which was implemented in #55 and reverted in #82.

## Problem: the wheel and native selection cannot both work while the TUI owns the screen

`crates/otto/src/tui/mod.rs` starts the TUI on the alternate screen
(`ratatui::try_init`) and renders the whole transcript into it with an internal
scroll offset (`App::scroll`, `render::draw_transcript`). In that arrangement:

- Wheel notches reach Otto only while mouse reporting is on (`CSI ?1000h` and
  friends). With reporting on, terminals stop doing native drag-selection and
  require Shift (Option in iTerm2).
- With reporting off, the alternate screen has no scrollback for the terminal to
  scroll, so the wheel does nothing.

#136 tried to get both by enabling reporting only while `App::scroll.is_some()`
(`mouse_policy_for_scroll`). That state is reachable only through
`App::scroll_wheel` (needs reporting) or a scroll key, so from the idle state the
wheel produces no events at all. That is the reported regression.

Measurements in the reporter's environment (tmux `next-3.9`, tmux's own `mouse`
option off):

- tmux does not implement DECSET 1007; `input.c` handles 1000/1002/1003/1004/
  1005/1006/1049 and no 1007 case, so codex's `EnableAlternateScroll` escape is
  discarded and never forwarded to the outer terminal.
- tmux's default wheel binding is
  `bind -n WheelUpPane { if -F '#{||:#{alternate_on},...}' { send -M } { copy-mode -e } }`
  (`key-bindings.c:514`). For an alternate-screen pane it takes the `send -M`
  branch, and `input_key_mouse` (`input-keys.c`) drops the event when the pane
  has no mouse mode enabled.

So no escape sequence restores the wheel while Otto holds the alternate screen.
The transcript has to live in the terminal's own scrollback.

## Goal: the terminal owns the transcript, Otto owns a live region at the bottom

- No alternate screen, no mouse reporting, no `?1007`.
- Finished transcript entries are written into the terminal's scrollback.
  Selection and wheel scrolling are the terminal's native behavior.
- Otto draws only a live region at the bottom of the screen: the entries of the
  current turn that are not final yet, the thinking line, the footer, the slash
  suggestions, and the composer.

Non-goals in this iteration: line-level streaming commits, re-rendering
committed lines on resize, an alternate-screen transcript overlay, and vendoring
a ratatui terminal.

## Design

### 1. Live region: a `Viewport::Fixed` rect pinned above the screen bottom

ratatui 0.30.2 cannot resize a `Viewport::Inline(height)` after construction
(`set_viewport_area` is `pub(crate)`; `resize` recomputes an inline viewport from
a cursor-position query). `Terminal::clear` issues `get_cursor_position()` for every
viewport kind and `Terminal::resize` does so for inline ones, and a
cursor-position query races the TUI's dedicated `crossterm::event::read()`
thread for the terminal's reply.

`Viewport::Fixed(Rect)` avoids all of it: `with_options` takes the rect as given,
`autoresize` skips fixed viewports, and `resize(rect)` assigns the new rect and
clears it without querying the cursor. A new `tui::inline::LiveRegion` owns:

- `top: u16` — the first screen row the live region occupies.
- `height: u16` — rows the live region needs this frame.
- `Terminal<CrosstermBackend<Stdout>>` with `Viewport::Fixed(Rect::new(0, top,
  width, height))`.

Invariant: `top == min(anchor, screen_height - height)`, where `anchor` starts at
the cursor row read once at startup (before the key-reader thread is spawned, so
no query races the reader) and moves down as scrollback is written. Once the
screen has filled, the live region is pinned to the bottom `height` rows.

Every frame the caller computes the needed height from the same layout inputs
`render::draw` already uses (live entries, thinking line, footer, suggestions,
composer, clamped to the screen). When the height, the width, or the screen size
changes, `LiveRegion` clears from the old `top` down and calls
`Terminal::resize` with the new rect; for a fixed viewport that assigns the area
and clears it with no cursor query, and because the rect is full-width and
bottom-anchored the clear is a single erase-below (`clear_fixed_viewport`).
`Terminal::clear()` is never called.

### 2. Scrollback writes go through the backend, not through `Terminal`

`Terminal::insert_before` refuses to run on anything but an inline viewport, so
`LiveRegion::commit(rows)` ports its `insert_before_no_scrolling_regions` loop:
draw at most a screenful per pass, scroll only as far as that pass needs
(`scroll_up = max(0, drawn + to_draw - screen_height)`), and repeat. Scrolling
is `Backend::append_lines` from the bottom row, which writes plain `\n`;
drawing is `Backend::draw`, which already converts cells to SGR. Nothing writes
raw ANSI by hand and nothing tracks wrapped row counts.

`rows` is a `Buffer` produced by `render::committed_buffer`, which renders the
committed entries through the same `Paragraph` and wrapping the live region
uses, so committed and live output are identical. Its height comes from
`Paragraph::line_count`, available through the `unstable-rendered-line-info`
feature.

### 3. Commit rule: the longest final prefix of `App::entries`

`App` gains `committed: usize`. Entry `i` is final when it is not the last entry
of a running turn and, for a tool entry, `tool_done` is set:

```
final(i) = (i + 1 < entries.len() || !busy)
        && (entries[i].kind != Tool || entries[i].tool_done)
```

`App::take_committable()` advances `committed` over the longest prefix of final
entries and returns them, preserving order (a pending parallel tool call blocks
the entries behind it). The live region renders `entries[committed..]`.

Commit points: after each `apply_event` batch inside `drive_turn`, after
`push_system`, at the end of every turn, and after `App::refresh` — which
replaces the whole transcript (`/new`, `/resume`, `/archive`, profile switch) and
therefore resets `committed` to 0 so the loaded history prints into scrollback.

### 4. Overlays render inside the live region

`/resume`, `/archive`, `/model` (`App::picker`) and the help overlay currently
draw as centered popups over the full screen. They become live-region content
rendered at their natural height, clamped to the screen height, which grows the
live region while one is open and shrinks it back on close.

### 5. Deletions

- `MousePolicy`, `TerminalInput`, `mouse_policy_for_scroll`, `sync_mouse_policy`,
  and the `EnableMouseCapture`/`DisableMouseCapture` calls (#136).
- `TuiEvent::Wheel` and its mapping in `map_terminal_event`.
- `App::scroll`, `App::max_scroll`, `scroll_up`, `scroll_down`, `scroll_wheel`,
  `handle_scroll_key`, and the `PageUp`/`PageDown` key routing.
- `ratatui::try_init`/`try_restore` (alternate screen). Raw mode and bracketed
  paste are set up directly.

## Accepted regressions

These are the same ones the Go design accepted, plus the two that come from the
fixed-viewport mechanics:

1. `Ctrl+O` (`show_details`) applies only to entries that have not been committed
   yet; already-printed tool blocks keep the form they were printed in.
2. Committed lines are not re-rendered on resize. The terminal reflows them or
   does not, as it chooses.
3. `PageUp`/`PageDown` and the wheel no longer scroll inside Otto. Scrollback is
   the terminal's.
4. `/new` does not clear scrollback; `/resume` prints the loaded session's
   history again.
5. Closing an overlay leaves the rows it used blank above the live region until
   new output pushes the region down.
6. A streaming entry taller than the live region shows only its tail.
7. Otto's last frame stays on screen after exit; there is no alternate screen to
   restore.

## Verification

Focused tests first, in `crates/otto/src/tui/`:

- `App::take_committable` over: a streaming assistant entry, a pending tool
  followed by a finished one, a finished turn, and a `refresh` reset.
- `LiveRegion` height selection and the `top` invariant against a `Vec<u8>`
  writer and a `TestBackend`.
- `LiveRegion::commit` against a `TestBackend`: rows land above the region,
  push it down, and scroll into scrollback once it is at the bottom.
- A guard asserting the TUI emits neither `?1049h` nor `?1000h`.

`LiveRegion::new` reads the cursor position, and a terminal that never answers
the query must not keep the TUI from starting; the fallback assumes the cursor
is past the last row, which scrolls the region into place instead of writing
over what is on screen.

Then `crates/otto/tests/tui_pty.rs`: the scraper currently treats `CSI ?1049h` as
a grid reset and rejects unknown sequences; it needs erase-in-display,
erase-in-line, and index/scroll handling, and the assertions become "no alternate
screen, no mouse capture, transcript text present on screen, prompt reply
visible".

Gates: `make check-fast` during development, `make check` before the pull
request.
