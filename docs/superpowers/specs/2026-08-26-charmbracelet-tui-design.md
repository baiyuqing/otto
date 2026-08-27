# Otto Charmbracelet TUI Design

**Date:** 2026-08-26  
**Status:** Approved

## 1. Purpose

Add a production-quality, Pi-inspired terminal UI to Otto using the Charmbracelet ecosystem while preserving the existing line-oriented REPL for pipes, automation, and non-TTY environments.

The default UI mode is adaptive: interactive terminals receive the full-screen TUI, while non-interactive input or output uses the existing REPL.

## 2. Scope

### Included

- Bubble Tea full-screen application
- Bubbles textarea, viewport, and spinner
- Lip Gloss adaptive styling
- Glamour Markdown rendering
- Adaptive TUI/REPL selection
- Scrollable transcript
- Multiline prompt editor
- Streaming assistant content
- Inline collapsible tool calls and results
- Status footer
- Help and session overlays
- Prompt cancellation
- Session creation and resume history rendering
- Shared application controller for both frontends
- Pseudo-terminal smoke coverage

### Excluded

- Model or profile selector overlays
- Session browser
- Message queueing while a turn is active
- Transcript search
- Custom theme files
- Inline/non-alternate-screen mode
- Mouse-driven selection or editing
- Codex or Claude provider implementation

## 3. User Interface Selection

Add:

```text
--ui auto|tui|repl
```

Resolution precedence is:

1. `--ui`
2. `OTTO_UI`
3. TOML `[ui].mode`
4. `auto`

Example configuration:

```toml
[ui]
mode = "auto"
```

Behavior:

- `auto` selects TUI only when both stdin and stdout are terminals.
- `auto` selects REPL for piped or redirected operation.
- `tui` requires terminal stdin and stdout and otherwise fails with actionable guidance.
- `repl` always selects the existing line-oriented frontend.
- Help and invalid UI mode handling occur before session creation or terminal raw-mode initialization.

TTY detection is injected in command tests rather than coupled directly to process-global descriptors.

## 4. Architecture

```text
cmd/otto
   +-- resolves config, provider, tools, and UI mode
   +-- handles process signals
   +-- constructs app.Controller
             |
             +-- internal/tui
             +-- internal/repl

internal/app.Controller
   +-- current Session
   +-- current Agent
   +-- Prompt(ctx, text, emit)
   +-- NewSession()
   +-- Info()
   +-- History()
   +-- Close()
```

### `internal/app`

The shared controller owns session and agent lifecycle. It:

- Delegates prompts to the current agent.
- Rejects concurrent prompt execution.
- Creates and swaps sessions safely.
- Rebuilds the agent after session replacement.
- Exposes provider-neutral history and metadata.
- Preserves fatal-persistence classification.
- Keeps credentials out of frontend state.
- Closes resources idempotently.

Conceptual API:

```go
type Controller interface {
    Prompt(ctx context.Context, text string, emit func(agent.Event)) error
    NewSession() error
    Info() Info
    History() []model.Message
    Close() error
}
```

### `internal/tui`

The TUI owns only presentation state and frontend interaction. It never reads provider wire types, credentials, or JSONL storage directly.

### `internal/repl`

The existing REPL remains supported and is adapted to the shared controller. Its non-TTY behavior and commands remain compatible.

### `cmd/otto`

The command package retains:

- CLI flags
- Configuration resolution
- TTY detection
- Provider/tool construction
- UI selection
- Process signal lifecycle

Session and agent replacement logic moves out of the command package into `internal/app`.

## 5. Charmbracelet Components

- **Bubble Tea:** application model, commands, messages, and update loop
- **Bubbles:** textarea, viewport, and spinner
- **Lip Gloss:** responsive layout and light/dark adaptive styling
- **Glamour:** Markdown rendering
- **`golang.org/x/term`:** terminal detection

Dependencies are pinned to stable compatible releases during implementation.

The TUI uses the alternate screen buffer. Terminal state must be restored on normal exit, cancellation, fatal application errors, and initialization failures.

## 6. TUI Model

The Bubble Tea model owns:

- Transcript entries
- Viewport and scroll state
- Multiline textarea
- Spinner state
- Active-turn state
- Session/provider/model metadata
- Input/output token totals
- Tool expansion state
- Help/session overlay state
- Active cancellation function
- Terminal width and height
- Double-`Ctrl+C` timing state

Transcript entry kinds are:

- User message
- Assistant message
- Tool call/result
- Error
- System/status

Completed entries cache their rendered representation. The active assistant entry is mutable while streaming.

## 7. Agent Event Flow

1. The user submits a prompt.
2. The TUI appends a user entry.
3. A Bubble Tea command starts `Controller.Prompt` with a child context.
4. Agent events are written to a bounded channel.
5. A waiting Bubble Tea command converts one channel value into a message.
6. `Update` applies the message and schedules the next wait.
7. A completion message stops the spinner and restores idle submission.

No worker goroutine mutates the Bubble Tea model. All presentation-state mutation happens in `Update`.

### Streaming

- Text deltas append to the active assistant entry.
- Markdown rendering is refreshed in timed batches rather than per token.
- Completion triggers an immediate final render.
- Tool start creates an inline running entry.
- Tool completion changes the entry to a one-line summary while retaining full output.
- `Ctrl+O` globally expands or collapses tool details.

### Scroll behavior

- The viewport follows new content only when the user is already near the bottom.
- Scrolling upward disables auto-follow.
- Jumping to the bottom restores auto-follow.
- Resize events preserve a valid viewport position.

### Input during a turn

The textarea remains editable while a turn runs, allowing the next prompt to be drafted. Submission is disabled until the active turn completes. Message queueing is not part of this milestone.

## 8. Layout

```text
+ Otto -------------------------------------------------+
|                                                        |
|  You                                                   |
|  Inspect the repository                                |
|                                                        |
|  Otto                                                  |
|  I'll inspect the project structure...                 |
|                                                        |
|  > read  internal/app/controller.go          complete  |
|                                                        |
+--------------------------------------------------------+
| prompt editor                                          |
|                                                        |
+--------------------------------------------------------+
| cwd | profile/model | input/output tokens | session    |
+--------------------------------------------------------+
```

- Transcript consumes available space.
- Editor grows from one to six lines.
- Footer removes lower-priority fields on narrow terminals.
- Very small terminals show a resize message instead of a corrupt layout.
- Adaptive colors must remain legible on light and dark terminal backgrounds.
- Mouse wheel and page keys scroll the transcript.

## 9. Keybindings

- `Enter`: submit
- `Shift+Enter` or `Alt+Enter`: insert newline
- `Esc`: cancel active turn or close overlay
- `Ctrl+O`: expand/collapse tool output
- `PgUp` / `PgDn`: scroll transcript
- `Home` / `End`: transcript top/bottom when not consumed by editor navigation
- First `Ctrl+C`: cancel active work or clear editor
- Second `Ctrl+C` within a short interval: exit
- `?`: open help when editor is empty

Terminal protocols do not distinguish every modified Enter sequence consistently. `Alt+Enter` is the guaranteed multiline fallback when `Shift+Enter` cannot be distinguished.

## 10. Commands and Overlays

- `/help`: open keybinding/command help
- `/new`: create a fresh session and clear transcript after creation succeeds
- `/session`: display session ID, path, provider, model, and profile
- `/exit`: quit

Commands are handled by the frontend/controller and are never sent to the model.

`/new` is rejected while a prompt is active. Session replacement must not discard the current session unless creating the replacement succeeds.

## 11. Resume and History Rendering

For `--continue` and `--resume`:

- User/assistant messages populate the initial transcript.
- Assistant Markdown is rendered and cached.
- Tool calls/results become collapsed inline entries.
- Errors remain visibly marked.
- Input/output token totals are reconstructed from assistant usage fields.
- The viewport begins at the newest message.

Provider-neutral messages are the only source for history rendering.

## 12. Error and Cancellation Semantics

- `Esc` cancels the active prompt context.
- The running provider/tool sees cancellation through the existing agent flow.
- Durable cancellation tool results remain persisted by the agent.
- Fatal persistence errors display once and terminate the frontend.
- Normal provider/tool errors leave the TUI usable.
- Markdown-rendering errors fall back to escaped plain text.
- Bubble Tea startup/runtime errors are returned to `cmd/otto` after terminal restoration.
- Closing the controller is idempotent.

First `Ctrl+C` behavior:

- Active turn: cancel it.
- Idle with nonempty editor: clear it.
- Idle with empty editor: arm exit.

A second `Ctrl+C` during the armed interval exits.

## 13. Performance

- Cache completed Markdown and layout output.
- Batch streaming renders on a short timer.
- Do not rebuild every historical entry for each token.
- Keep full tool output in state but omit it from collapsed layout.
- Re-render cached entries only when terminal width changes.
- Bound agent-event channels so cancellation and shutdown cannot leak goroutines.

## 14. Testing

### Application controller

- Prompt and event delegation
- Concurrent-prompt rejection
- New-session replacement and close ordering
- Resume history snapshots
- Fatal-persistence propagation
- Idempotent close

### TUI update model

- Initial history rendering
- Streaming text batching and completion
- Tool start/result transitions
- Markdown fallback
- Scroll preservation and auto-follow
- Resize behavior
- Submit and newline key behavior
- Drafting while active with submission disabled
- Active cancellation
- Double `Ctrl+C`
- Help/session overlays
- `/new`, `/exit`, and fatal errors
- Small-terminal fallback

### CLI

- `--ui`, `OTTO_UI`, and TOML precedence
- Automatic TTY/non-TTY selection
- Forced-TUI non-TTY rejection
- Existing REPL fallback
- Config/session errors before TUI startup

### Integration

A pseudo-terminal smoke test verifies:

- Alternate-screen startup
- Prompt input
- Streaming output
- Cancellation
- Resize handling
- Clean terminal restoration

Default tests remain offline and use fake providers/controllers.

## 15. Verification

```bash
go test ./...
go test -race ./...
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
test -z "$(gofmt -l .)"
git diff --check
```

README updates document adaptive UI behavior, full-screen mode, keybindings, REPL fallback, and the unsandboxed-shell warning.

The untracked `hello.py` in the original main checkout is unrelated and must remain untouched.
