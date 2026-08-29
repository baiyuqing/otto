# TUI Transcript Clarity Design

**Date:** 2026-08-29
**Status:** Implemented

## Goal

Make conversation turns and assistant-owned tool activity immediately distinguishable without surrounding every entry with a full border.

## Visual hierarchy

Use spacing, background, indentation, and labels as a small visual grammar:

- A user message is a subtle background band labeled `You`.
- An assistant turn uses the normal terminal background and is labeled `Otto`.
- Tool activity is indented beneath the assistant turn that owns it.
- `>` is reserved for the bottom input prompt. Transcript entries never use it.
- Color is optional reinforcement, never the only distinction.

The user band may span the transcript width, while prose remains limited to a readable 100-cell content width. When background color is unavailable, the band falls back to a left rail plus the `You` label.

## Turn grouping

A user message starts a user block. All following assistant text and tool calls belong to one assistant turn until the next user message.

Assistant text may arrive before and after a tool call; it remains in the same visual group. A tool call without surrounding assistant text still renders as assistant-owned activity.

Completed transcript example:

```text
[subtle background]
  You
  Delete the two stale branches.
[/subtle background]

Otto
I’ll remove them.

  ✓ bash  git branch -d docs/old feature/old…

Both branches were deleted.
```

While the command is running, the same activity row uses `…` instead of `✓`.

## Tool activity

Tool state appears at the beginning of the activity row so it cannot be mistaken for command text:

- `… bash  <command>`: running
- `✓ bash  <command>`: completed successfully
- `✗ bash  <command>`: failed

Rules:

- Never prefix a tool call with `>`.
- Never append `running`, `complete`, or `error` to the command text.
- Show a human-readable display value rather than a raw JSON wrapper.
- For `bash`, decode and display the `command` value.
- Unknown or malformed tool arguments use the existing safely escaped compact fallback.
- A collapsed activity row is one line and uses at most 120 terminal cells or the available width after indentation, whichever is smaller.
- `Ctrl+O` reveals the complete escaped arguments and output.
- Collapsed successful calls hide routine output. Failed calls keep a concise error visible even while collapsed.

## Input prompt

The bottom editor keeps the shell-like prompt:

```text
> Ask Otto
```

This is the only place where `>` means user input. Direct user shell mode, if added later, must use a separate explicit mode and is outside this spec.

## Responsive and accessible behavior

- User and assistant prose wraps within 100 cells or the available width when narrower.
- Tool rows truncate with an ellipsis and never rely on terminal autowrap.
- Labels (`You`, `Otto`, and the tool name) remain visible without color.
- Status symbols remain distinct in monochrome terminals.
- Streaming, resize, history resume, and Markdown rendering preserve the same grouping.
- Very small terminals continue to use the existing resize message.

## Scope

Change TUI transcript presentation only. Do not change stored sessions, agent events, tool execution, provider behavior, keybindings, themes, or the REPL.

Do not add full message borders, a configurable width system, a split activity pane, or a general per-tool renderer framework.

## Acceptance tests

1. Adjacent user and assistant turns have visibly different structure without full borders.
2. User background-band fallback remains identifiable without color.
3. Assistant text before and after a tool call stays in one assistant-owned visual group.
4. `>` appears only in the editor, never on a tool activity row.
5. Running, successful, and failed tool rows use distinct leading states.
6. A collapsed bash row contains the decoded command, not its JSON wrapper or a trailing status word.
7. Long tool rows remain single-line and bounded; `Ctrl+O` exposes complete arguments and output.
8. Narrow terminals, wide characters, streaming, resize, and resumed history stay within bounds and preserve ordering.

## References

- Codex separates user messages with a dedicated background treatment and uses nested command/output rows: <https://github.com/openai/codex/blob/main/codex-rs/tui/src/history_cell/messages.rs>
- Gemini CLI supports compact tool output and clean display titles rather than raw metadata: <https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/tools/tools.ts>
- Pi separates tool title, muted arguments, result state, and expanded details: <https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/extensions.md>
