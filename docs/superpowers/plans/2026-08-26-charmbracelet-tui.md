# Otto Charmbracelet TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an adaptive, full-screen Charmbracelet TUI to Otto while preserving the existing REPL for non-TTY use.

**Architecture:** Extract agent/session lifecycle into `internal/app.Controller`, then make REPL and TUI independent frontends over that controller. The Bubble Tea model receives typed agent events through bounded channels and mutates presentation state only in `Update`; `cmd/otto` resolves configuration, chooses `auto|tui|repl`, and owns process wiring.

**Tech Stack:** Go 1.26; `charm.land/bubbletea/v2` v2.0.9; `charm.land/bubbles/v2` v2.2.1; `charm.land/lipgloss/v2` v2.0.6; `charm.land/glamour/v2` v2.0.1; `golang.org/x/term` v0.45.0; `github.com/creack/pty` v1.1.24 for macOS pseudo-terminal tests; standard `testing` and existing Otto packages.

## Global Constraints

- Work only on `feature/charmbracelet-tui` in `.worktrees/charmbracelet-tui`.
- Keep Stage 1 provider support limited to `openai-compatible`; Codex and Claude remain planned.
- Default `--ui auto`: TUI only when stdin and stdout are terminals; otherwise REPL.
- Forced `--ui tui` on non-TTY input/output fails before session creation and raw terminal mode.
- TUI uses Bubble Tea v2 alternate-screen mode and restores the terminal on every exit path.
- Preserve existing workspace, credential-redaction, fatal-persistence, session, and unsandboxed-shell guarantees.
- No frontend may depend on provider-specific wire types, credentials, or JSONL records.
- All Bubble Tea model mutation occurs in `Update`; worker goroutines communicate through messages/channels.
- Editor remains writable during a turn, but submission is disabled; message queueing is out of scope.
- Use exact Pi-like keys from the spec, with `Alt+Enter` as the reliable multiline fallback.
- Default tests remain offline; no real provider credentials or network calls.
- Use TDD and one focused commit per task.
- Leave the unrelated `hello.py` in the original main checkout untouched.

---

## Planned File Structure

```text
cmd/otto/main.go                   UI selection and dependency wiring
cmd/otto/main_test.go
cmd/otto/tui_pty_test.go           darwin pseudo-terminal smoke test
internal/app/controller.go         shared session/agent lifecycle
internal/app/controller_test.go
internal/config/config.go          [ui] TOML section
internal/config/config_test.go
internal/config/ui.go              UI mode resolution
internal/config/ui_test.go
internal/repl/repl.go              controller-backed fallback frontend
internal/repl/repl_test.go
internal/tui/entries.go            provider-neutral history/transcript model
internal/tui/entries_test.go
internal/tui/keymap.go             Pi-like key bindings
internal/tui/layout.go             responsive viewport/editor/footer/overlays
internal/tui/markdown.go           Glamour renderer and plain-text fallback
internal/tui/messages.go           Bubble Tea messages and turn channel
internal/tui/model.go              Init/Update/View state machine
internal/tui/model_test.go
internal/tui/run.go                Bubble Tea program construction
internal/tui/run_test.go
README.md                           adaptive UI and keybinding documentation
AGENTS.md                           updated package boundary and commands
go.mod
go.sum
```

---

### Task 1: Add UI Configuration

**Files:**
- Modify: `internal/config/config.go`
- Modify: `internal/config/config_test.go`
- Create: `internal/config/ui.go`
- Create: `internal/config/ui_test.go`

**Interfaces:**
- Produces: `config.UIMode`, `config.ResolveUIMode(file, env, override)`, and strict `[ui].mode` TOML decoding.
- Consumes: existing strict TOML loader.

- [ ] **Step 1: Write failing UI-mode tests**

Create table tests covering defaults, precedence, and rejection:

```go
func TestResolveUIModePrecedence(t *testing.T) {
    file := File{UI: UI{Mode: "repl"}}
    tests := []struct {
        name     string
        env      map[string]string
        override string
        want     UIMode
    }{
        {name: "toml", want: UIRepl},
        {name: "environment", env: map[string]string{"OTTO_UI": "tui"}, want: UITUI},
        {name: "cli", env: map[string]string{"OTTO_UI": "tui"}, override: "auto", want: UIAuto},
    }
    for _, test := range tests {
        t.Run(test.name, func(t *testing.T) {
            got, err := ResolveUIMode(file, test.env, test.override)
            if err != nil {
                t.Fatal(err)
            }
            if got != test.want {
                t.Fatalf("mode = %q, want %q", got, test.want)
            }
        })
    }
}

func TestResolveUIModeRejectsUnknownValue(t *testing.T) {
    _, err := ResolveUIMode(File{}, map[string]string{"OTTO_UI": "graphical"}, "")
    if err == nil || !strings.Contains(err.Error(), "auto, tui, repl") {
        t.Fatalf("unexpected error: %v", err)
    }
}
```

Also extend strict TOML tests to decode `[ui]\nmode = "auto"` and reject unknown UI keys.

- [ ] **Step 2: Run RED**

```bash
go test ./internal/config -run 'Test(ResolveUIMode|LoadUI)'
```

Expected: FAIL because UI types and resolver do not exist.

- [ ] **Step 3: Implement strict UI-mode resolution**

Define:

```go
type UIMode string

const (
    UIAuto UIMode = "auto"
    UITUI  UIMode = "tui"
    UIRepl UIMode = "repl"
)

type UI struct {
    Mode string `toml:"mode"`
}

func ResolveUIMode(file File, env map[string]string, override string) (UIMode, error)
```

Resolution is CLI override, then `OTTO_UI`, then TOML, then `auto`. Trim surrounding whitespace, lowercase values, and return an actionable error listing all valid values.

- [ ] **Step 4: Run GREEN and current suite**

```bash
gofmt -w internal/config
go test ./internal/config
go test ./...
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add internal/config
git commit -m "feat: add UI mode configuration"
```

---

### Task 2: Extract Shared Application Controller

**Files:**
- Create: `internal/app/controller.go`
- Create: `internal/app/controller_test.go`

**Interfaces:**
- Consumes: `session.Session`, `model.Message`, `agent.Event`.
- Produces: `app.Backend`, `app.Controller`, `app.Info`, `app.Runner`, `app.SessionFactory`, and `app.RunnerFactory`.

- [ ] **Step 1: Write lifecycle tests first**

Define fakes and test the public behavior:

```go
func TestControllerRejectsConcurrentPrompt(t *testing.T) {
    started := make(chan struct{})
    release := make(chan struct{})
    runner := runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
        close(started)
        <-release
        return nil
    })
    controller := newTestController(t, runner)
    done := make(chan error, 1)
    go func() { done <- controller.Prompt(context.Background(), "one", func(agent.Event) {}) }()
    <-started
    if err := controller.Prompt(context.Background(), "two", func(agent.Event) {}); !errors.Is(err, ErrPromptActive) {
        t.Fatalf("error = %v, want ErrPromptActive", err)
    }
    close(release)
    if err := <-done; err != nil {
        t.Fatal(err)
    }
}

func TestControllerCreatesReplacementBeforeClosingCurrent(t *testing.T) {
    var order []string
    current := &fakeSession{header: testHeader("old"), onClose: func() { order = append(order, "close-old") }}
    next := &fakeSession{header: testHeader("new")}
    controller, err := New(current, func() (session.Session, error) {
        order = append(order, "create-new")
        return next, nil
    }, func(session.Session) Runner { return runnerFunc(noopRun) })
    if err != nil {
        t.Fatal(err)
    }
    if err := controller.NewSession(); err != nil {
        t.Fatal(err)
    }
    if !reflect.DeepEqual(order, []string{"create-new", "close-old"}) {
        t.Fatalf("order = %v", order)
    }
    if controller.Info().SessionID != "new" {
        t.Fatalf("info = %#v", controller.Info())
    }
}

type runnerFunc func(context.Context, string, func(agent.Event)) error
func (f runnerFunc) Run(ctx context.Context, text string, emit func(agent.Event)) error { return f(ctx, text, emit) }

func noopRun(context.Context, string, func(agent.Event)) error { return nil }

func testHeader(id string) session.Header {
    return session.Header{Version: 1, ID: id, Workspace: "/workspace", Provider: "openai-compatible", Profile: "test", Model: "model", CreatedAt: time.Unix(1, 0).UTC()}
}

type fakeSession struct {
    header   session.Header
    messages []model.Message
    closed   bool
    onClose  func()
}
func (f *fakeSession) Header() session.Header { return f.header }
func (f *fakeSession) Messages() []model.Message { return append([]model.Message(nil), f.messages...) }
func (f *fakeSession) Append(context.Context, model.Message) error { return nil }
func (f *fakeSession) Path() string { return "/sessions/" + f.header.ID + ".jsonl" }
func (f *fakeSession) Close() error {
    if !f.closed && f.onClose != nil { f.onClose() }
    f.closed = true
    return nil
}

func newTestController(t *testing.T, runner Runner) *Controller {
    t.Helper()
    controller, err := New(&fakeSession{header: testHeader("initial")}, func() (session.Session, error) {
        return &fakeSession{header: testHeader("next")}, nil
    }, func(session.Session) Runner { return runner })
    if err != nil { t.Fatal(err) }
    return controller
}
```

Also test creation failure retains the old session, new-session rejection while prompting, event forwarding, defensive history snapshots, idempotent close, close waiting for an active prompt, fatal-persistence identity, and methods after close.

- [ ] **Step 2: Run RED**

```bash
go test ./internal/app
```

Expected: FAIL because package does not exist.

- [ ] **Step 3: Implement the controller contract**

```go
type Runner interface {
    Run(context.Context, string, func(agent.Event)) error
}

type SessionFactory func() (session.Session, error)
type RunnerFactory func(session.Session) Runner

func New(initial session.Session, create SessionFactory, build RunnerFactory) (*Controller, error)

type Info struct {
    SessionID   string
    SessionPath string
    Workspace   string
    Provider    string
    Profile     string
    Model       string
}

type Backend interface {
    Prompt(context.Context, string, func(agent.Event)) error
    NewSession() error
    Info() Info
    History() []model.Message
}
```

`Controller` holds current session/runner, factories, `prompting`, `closed`, and an active-done channel under a mutex. `Prompt` captures the current runner, marks active, unlocks for execution, and clears/ closes active-done in `defer`. `Close` marks closed, waits outside the lock for an active prompt, then closes the current session exactly once.

`NewSession` checks idle/closed, creates the replacement before closing the old session, then builds/swaps the runner. If creation fails, old state remains. If closing old fails, close the replacement, mark the controller closed because the old session's durability state is uncertain, and return the close error without exposing either session for further prompts.

- [ ] **Step 4: Run GREEN and race tests**

```bash
gofmt -w internal/app
go test ./internal/app
go test -race ./internal/app
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add internal/app
git commit -m "refactor: add shared application controller"
```

---

### Task 3: Migrate REPL and CLI to the Controller Without Changing Default Behavior

**Files:**
- Modify: `internal/repl/repl.go`
- Modify: `internal/repl/repl_test.go`
- Modify: `cmd/otto/main.go`
- Modify: `cmd/otto/main_test.go`

**Interfaces:**
- Consumes: `app.Backend` and concrete `*app.Controller` for final close.
- Produces: controller-backed REPL and simpler command lifecycle, still REPL-only in this task.

- [ ] **Step 1: Write failing REPL/controller tests**

Replace static runner/info fakes with a backend fake. Add:

```go
func TestREPLNewSessionUsesBackendAndContinuesInput(t *testing.T) {
    backend := &fakeBackend{info: app.Info{SessionID: "old"}}
    backend.newSession = func() error {
        backend.info.SessionID = "new"
        return nil
    }
    input := strings.NewReader("/new\n/session\n/exit\n")
    var output bytes.Buffer
    console := New(input, &output, &output, backend)
    if err := console.Run(context.Background()); err != nil {
        t.Fatal(err)
    }
    if backend.newCalls != 1 || !strings.Contains(output.String(), "ID: new") {
        t.Fatalf("newCalls=%d output=%q", backend.newCalls, output.String())
    }
}

type fakeBackend struct {
    info       app.Info
    newCalls   int
    newSession func() error
    prompt     func(context.Context, string, func(agent.Event)) error
}
func (f *fakeBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
    if f.prompt == nil { return nil }
    return f.prompt(ctx, text, emit)
}
func (f *fakeBackend) NewSession() error {
    f.newCalls++
    if f.newSession == nil { return nil }
    return f.newSession()
}
func (f *fakeBackend) Info() app.Info { return f.info }
func (f *fakeBackend) History() []model.Message { return nil }
```

Add command tests proving one controller/session closes on normal exit, fatal error, cancellation, and `/new`; existing config/provider/tool/session tests must remain unchanged.

- [ ] **Step 2: Run RED**

```bash
go test ./internal/repl ./cmd/otto
```

Expected: FAIL because constructors still require runner/static info and `/new` returns a sentinel.

- [ ] **Step 3: Adapt REPL**

Use:

```go
func New(stdin io.Reader, stdout, stderr io.Writer, backend app.Backend) *REPL
func NewWithInput(input *Input, stdout, stderr io.Writer, backend app.Backend) *REPL
```

`Run` calls `backend.Prompt`. `/session` reads `backend.Info()` at command time. `/new` rejects while the turn is active through backend semantics, calls `backend.NewSession`, prints the new session ID, and continues with the same scanner. Remove `ErrNewSession`.

- [ ] **Step 4: Refactor command wiring**

Construct initial session exactly as today, then create:

```go
controller, err := app.New(initialSession, sessionFactory, func(current session.Session) app.Runner {
    client := openaicompat.New(runtime.BaseURL, runtime.APIKey, nil)
    return agent.New(client, registry, current, agent.Options{
        Model: runtime.Model, SystemPrompt: systemPrompt, MaxTurns: runtime.MaxTurns,
    })
})
```

Defer `controller.Close()`. Remove the outer `/new` REPL reconstruction loop and current-session mutable closure. Keep existing signal routing to `REPL.Interrupt`.

- [ ] **Step 5: Run all regression gates**

```bash
gofmt -w internal/repl cmd/otto
go test ./internal/repl ./cmd/otto
go test -race ./internal/repl ./cmd/otto
go test ./...
```

Expected: PASS with REPL still the only selected frontend.

- [ ] **Step 6: Commit**

```bash
git add internal/repl cmd/otto
git commit -m "refactor: share app lifecycle across frontends"
```

---

### Task 4: Build Provider-Neutral Transcript and Markdown Rendering

**Files:**
- Create: `internal/tui/entries.go`
- Create: `internal/tui/entries_test.go`
- Create: `internal/tui/markdown.go`
- Create: `internal/tui/markdown_test.go`

**Interfaces:**
- Consumes: `model.Message`, `model.Block`, `model.Usage`.
- Produces: transcript `Entry`, `EntriesFromHistory`, usage totals, and `MarkdownRenderer`.

- [ ] **Step 1: Write history conversion tests**

```go
func TestEntriesFromHistoryPairsToolResults(t *testing.T) {
    history := []model.Message{
        {Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "inspect"}}},
        {Role: model.RoleAssistant, Usage: &model.Usage{InputTokens: 10, OutputTokens: 4}, Blocks: []model.Block{
            {Type: model.BlockText, Text: "checking"},
            {Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
        }},
        {Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-1", ToolName: "read", Text: "file contents"}}},
    }
    entries, usage := EntriesFromHistory(history)
    if len(entries) != 3 || entries[2].Kind != EntryTool || entries[2].ToolOutput != "file contents" || !entries[2].ToolDone {
        t.Fatalf("entries = %#v", entries)
    }
    if usage.InputTokens != 10 || usage.OutputTokens != 4 {
        t.Fatalf("usage = %#v", usage)
    }
}
```

Test orphan-safe history conversion, zero-block assistant responses, tool errors, multiple text blocks, stable IDs, and defensive copies.

- [ ] **Step 2: Run RED**

```bash
go test ./internal/tui -run 'Test(Entries|Markdown)'
```

Expected: FAIL because TUI package does not exist.

- [ ] **Step 3: Add rendering dependencies and implement transcript types**

```bash
go get charm.land/lipgloss/v2@v2.0.6
go get charm.land/glamour/v2@v2.0.1
```

```go
type EntryKind string

const (
    EntryUser      EntryKind = "user"
    EntryAssistant EntryKind = "assistant"
    EntryTool      EntryKind = "tool"
    EntryError     EntryKind = "error"
    EntrySystem    EntryKind = "system"
)

type Entry struct {
    ID          string
    Kind        EntryKind
    Raw         string
    Rendered    string
    RenderWidth int
    ToolCallID  string
    ToolName    string
    ToolArgs    string
    ToolOutput  string
    ToolError   bool
    ToolDone    bool
}
```

Pair tool results by call ID without dropping malformed-but-loaded display data. Sum usage with overflow-safe nonnegative addition.

- [ ] **Step 4: Implement Markdown renderer with fallback**

```go
type MarkdownRenderer interface {
    Render(markdown string, width int) (string, error)
}

type GlamourRenderer struct{}
```

Create a new Glamour renderer per width using `glamour.WithWordWrap(max(20,width))` and an adaptive style selected from terminal background information available through Lip Gloss. Strip only Glamour's trailing layout newline. A `renderMarkdown` helper returns escaped plain text plus a nonfatal error marker when rendering fails.

- [ ] **Step 5: Run GREEN**

```bash
gofmt -w internal/tui
go test ./internal/tui -run 'Test(Entries|Markdown)'
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add internal/tui go.mod go.sum
git commit -m "feat: add TUI transcript rendering"
```

---

### Task 5: Create the Full-Screen Bubble Tea Layout

**Files:**
- Create: `internal/tui/keymap.go`
- Create: `internal/tui/layout.go`
- Create: `internal/tui/messages.go`
- Create: `internal/tui/model.go`
- Create: `internal/tui/model_test.go`

**Interfaces:**
- Consumes: `app.Backend`, transcript entries, Bubbles components.
- Produces: `tui.Model` implementing Bubble Tea v2 `tea.Model` with initial history, resize, scrolling, overlays, and alternate-screen `View`.

- [ ] **Step 1: Write initial-model and layout tests**

Test model construction from backend history and terminal sizing:

```go
func TestWindowResizeProducesResponsiveLayout(t *testing.T) {
    model := newTestModel(t)
    updated, _ := model.Update(tea.WindowSizeMsg{Width: 100, Height: 30})
    got := updated.(Model)
    if got.viewport.Width() != 100 || got.viewport.Height() <= 0 {
        t.Fatalf("viewport = %dx%d", got.viewport.Width(), got.viewport.Height())
    }
    view := got.View()
    if !view.AltScreen || !strings.Contains(view.Content, "profile/model") {
        t.Fatalf("view = %#v", view)
    }
}

func TestSmallTerminalShowsResizeMessage(t *testing.T) {
    model := newTestModel(t)
    updated, _ := model.Update(tea.WindowSizeMsg{Width: 30, Height: 6})
    if content := updated.(Model).View().Content; !strings.Contains(content, "terminal is too small") {
        t.Fatalf("content = %q", content)
    }
}
```

Also test initial history at bottom, cached entry re-render on width change, narrow footer field removal, help/session overlay content, tool expansion toggle, and viewport scroll disabling auto-follow.

Define these shared test helpers in `model_test.go` for Tasks 5-7:

```go
type fakeBackend struct {
    prompt     func(context.Context, string, func(agent.Event)) error
    newSession func() error
    info       app.Info
    history    []model.Message
}

func (f *fakeBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
    if f.prompt == nil { return nil }
    return f.prompt(ctx, text, emit)
}
func (f *fakeBackend) NewSession() error {
    if f.newSession == nil { return nil }
    return f.newSession()
}
func (f *fakeBackend) Info() app.Info { return f.info }
func (f *fakeBackend) History() []model.Message { return append([]model.Message(nil), f.history...) }

type rendererFunc func(string, int) (string, error)
func (f rendererFunc) Render(text string, width int) (string, error) { return f(text, width) }

func keyPress(code rune, modifiers ...tea.KeyMod) tea.KeyPressMsg {
    var mod tea.KeyMod
    for _, modifier := range modifiers { mod |= modifier }
    return tea.KeyPressMsg(tea.Key{Code: code, Mod: mod})
}

func newTestModelWithBackend(t *testing.T, backend *fakeBackend) Model {
    t.Helper()
    return NewModel(context.Background(), backend, WithRenderer(rendererFunc(func(text string, _ int) (string, error) { return text, nil })))
}

func newTestModel(t *testing.T) Model {
    t.Helper()
    return newTestModelWithBackend(t, &fakeBackend{info: app.Info{Profile: "profile", Model: "model", SessionID: "session"}})
}
```

- [ ] **Step 2: Run RED**

```bash
go test ./internal/tui -run 'Test(Window|SmallTerminal|InitialHistory|Overlay|ToolExpansion|Scroll)'
```

Expected: FAIL because Bubble Tea model does not exist.

- [ ] **Step 3: Add Bubble Tea dependencies and implement keymap/model state**

```bash
go get charm.land/bubbletea/v2@v2.0.9
go get charm.land/bubbles/v2@v2.2.1
```

Create bindings with `charm.land/bubbles/v2/key`. Configure textarea's `InsertNewline` binding to `shift+enter` and `alt+enter`; intercept plain `enter` before forwarding to textarea.

Expose `func NewModel(ctx context.Context, backend app.Backend, options ...Option) Model`. Model fields include root context, backend, entries, viewport, textarea, spinner, keymap, width/height, usage, running, expandedTools, overlay, autoFollow, renderer, dirtyStreaming, cancel, and Ctrl-C armed timestamp. Initialize textarea focused, one line high, six-line maximum, and viewport mouse scrolling enabled.

- [ ] **Step 4: Implement responsive View**

`View()` returns `tea.View` with:

```go
view := tea.NewView(content)
view.AltScreen = true
view.MouseMode = tea.MouseModeCellMotion
view.KeyboardEnhancements.ReportEventTypes = false
view.KeyboardEnhancements.ReportAlternateKeys = true
```

Render transcript, editor, and footer with Lip Gloss. Recalculate editor height from hard lines plus `textarea.LineInfo().Height`, clamped to 1..6. The transcript receives remaining height. Overlays are centered within the same full-screen content.

- [ ] **Step 5: Run GREEN and race tests**

```bash
gofmt -w internal/tui
go test ./internal/tui
go test -race ./internal/tui
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add internal/tui go.mod go.sum
git commit -m "feat: add full-screen TUI layout"
```

---

### Task 6: Add Streaming Turns, Batching, and Cancellation

**Files:**
- Modify: `internal/tui/messages.go`
- Modify: `internal/tui/model.go`
- Modify: `internal/tui/model_test.go`

**Interfaces:**
- Consumes: `app.Backend.Prompt` and `agent.Event`.
- Produces: bounded turn channel, Bubble Tea commands/messages, streaming assistant/tool state, usage, and fatal handling.

- [ ] **Step 1: Write event-flow tests**

Use a fake backend and execute returned commands directly:

```go
func TestPromptCommandStreamsEventsAndCompletes(t *testing.T) {
    backend := &fakeBackend{prompt: func(ctx context.Context, text string, emit func(agent.Event)) error {
        emit(agent.Event{Type: agent.EventTextDelta, Text: "hello"})
        emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 3, OutputTokens: 2}})
        return nil
    }}
    m := newTestModelWithBackend(t, backend)
    m.editor.SetValue("question")
    updated, cmd := m.Update(keyPress(tea.KeyEnter))
    running := updated.(Model)
    if !running.running || running.editor.Value() != "" {
        t.Fatalf("running=%v editor=%q", running.running, running.editor.Value())
    }
    first := cmd()
    afterFirst, next := running.Update(first)
    if next == nil || len(afterFirst.(Model).entries) == 0 {
        t.Fatal("stream did not start")
    }
}
```

Test tool start/result transitions, draft editing while running, disabled submit, Esc cancellation, normal errors remaining usable, fatal persistence producing quit, bounded-channel shutdown without goroutine leak, and usage accumulation.

- [ ] **Step 2: Run RED**

```bash
go test ./internal/tui -run 'Test(Prompt|Tool|Draft|Escape|Fatal|TurnChannel|Usage)'
```

Expected: FAIL because turn commands are absent.

- [ ] **Step 3: Implement bounded turn messaging**

Define:

```go
type turnEnvelope struct {
    event *agent.Event
    err   error
    done  bool
}

type turnMsg struct {
    channel <-chan turnEnvelope
    value   turnEnvelope
}
```

Create a child context from the model's root context (`context.WithCancel(m.rootCtx)`) and a channel of capacity 64. The worker invokes backend `Prompt`; event callback sends with `select { case channel <- value: case <-ctx.Done(): }`. Completion sends exactly one done envelope and closes the channel without blocking after cancellation.

`waitTurn(channel)` blocks in a `tea.Cmd`. Every received non-done message schedules the next wait.

- [ ] **Step 4: Implement streaming render batching**

Text events update raw active-assistant text and mark it dirty. Schedule at most one 50 ms `tea.Tick`; the tick renders only the active entry and refreshes viewport content. Completion performs an immediate final render. Tool events update entries immediately.

When auto-follow was true before refresh, call `viewport.GotoBottom`; otherwise preserve Y offset.

- [ ] **Step 5: Implement cancellation/error semantics**

Esc calls active cancel but remains running until the done envelope arrives. Fatal persistence sets fatal state and returns `tea.Quit`; ordinary provider/tool errors create an error entry and return idle. Controller errors retain `errors.Is` behavior.

- [ ] **Step 6: Run GREEN and race tests**

```bash
gofmt -w internal/tui
go test ./internal/tui
go test -race ./internal/tui
```

Expected: PASS with no goroutine leak under repeated cancellation test.

- [ ] **Step 7: Commit**

```bash
git add internal/tui
git commit -m "feat: stream agent turns in TUI"
```

---

### Task 7: Implement Pi-Like Keys, Commands, and Overlays

**Files:**
- Modify: `internal/tui/keymap.go`
- Modify: `internal/tui/layout.go`
- Modify: `internal/tui/model.go`
- Modify: `internal/tui/model_test.go`

**Interfaces:**
- Produces: exact submit/newline, scrolling, tool expansion, help/session overlays, slash commands, `/new`, and double-Ctrl-C behavior.

- [ ] **Step 1: Write key and command tests**

Cover exact behavior:

```go
func TestEnterSubmitsAndAltEnterAddsNewline(t *testing.T) {
    m := newTestModel(t)
    m.editor.SetValue("one")
    updated, cmd := m.Update(keyPress(tea.KeyEnter, tea.ModAlt))
    withNewline := updated.(Model)
    if cmd == nil || !strings.Contains(withNewline.editor.Value(), "\n") {
        t.Fatalf("value = %q", withNewline.editor.Value())
    }
    withNewline.editor.SetValue("send")
    submitted, promptCmd := withNewline.Update(keyPress(tea.KeyEnter))
    if promptCmd == nil || !submitted.(Model).running {
        t.Fatal("enter did not submit")
    }
}
```

Also test Shift+Enter where enhanced keys are available, Ctrl+O, PgUp/PgDn, Home/End routing, `?`, `/help`, `/session`, `/new` idle success/failure, `/new` active rejection, `/exit`, first Ctrl+C active cancel, first idle Ctrl+C editor clear/arm, second Ctrl+C within one second quit, and expired arm reset.

- [ ] **Step 2: Run RED**

```bash
go test ./internal/tui -run 'Test(Enter|ShiftEnter|ToolToggle|Page|Help|Session|New|Exit|CtrlC)'
```

Expected: FAIL for unimplemented bindings and commands.

- [ ] **Step 3: Implement command routing before prompt submission**

Trim the editor only for command recognition. Preserve original prompt whitespace for normal model submission. Commands never enter transcript or reach backend Prompt.

`/new` calls backend synchronously in a `tea.Cmd`, then a result message replaces entries from fresh `History`, resets usage, updates info, and clears the editor only on success.

- [ ] **Step 4: Implement Ctrl-C state machine**

Use an injected clock in model options. First Ctrl+C:

- Running: cancel and arm.
- Idle with editor text: clear and arm.
- Idle empty: arm and show “press Ctrl+C again to exit”.

Second within one second returns `tea.Quit`. A tick clears expired armed state.

- [ ] **Step 5: Run GREEN**

```bash
gofmt -w internal/tui
go test ./internal/tui
go test -race ./internal/tui
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add internal/tui
git commit -m "feat: add TUI commands and keybindings"
```

---

### Task 8: Wire Adaptive TUI/REPL Selection in the CLI

**Files:**
- Create: `internal/tui/run.go`
- Create: `internal/tui/run_test.go`
- Modify: `cmd/otto/main.go`
- Modify: `cmd/otto/main_test.go`

**Interfaces:**
- Consumes: `config.ResolveUIMode`, `app.Backend`, TUI model.
- Produces: `tui.Run(ctx, input, output, backend)` and CLI `--ui` adaptive selection.

- [ ] **Step 1: Write runner and selection tests**

Add dependency hooks:

```go
type frontendKind string

const (
    frontendTUI  frontendKind = "tui"
    frontendREPL frontendKind = "repl"
)

type terminalDetector func(io.Reader, io.Writer) bool
```

Test `auto` chooses TUI only when detector is true, `repl` always chooses REPL, forced TUI false detector fails before `newSession`, and CLI > env > TOML precedence. Verify `--help` lists `--ui` without needing credentials.

- [ ] **Step 2: Run RED**

```bash
go test ./internal/tui ./cmd/otto -run 'Test(Run|UI|Auto|ForcedTUI|Help)'
```

Expected: FAIL because runner/selection are absent.

- [ ] **Step 3: Add terminal detection and implement `tui.Run`**

```bash
go get golang.org/x/term@v0.45.0
```

```go
func Run(ctx context.Context, input io.Reader, output io.Writer, backend app.Backend) error {
    program := tea.NewProgram(
        NewModel(ctx, backend),
        tea.WithContext(ctx),
        tea.WithInput(input),
        tea.WithOutput(output),
        tea.WithoutSignalHandler(),
    )
    _, err := program.Run()
    return err
}
```

The model's `View` owns alternate-screen/mouse declarations, so no v1-style `WithAltScreen` option is used.

- [ ] **Step 4: Implement TTY detection and CLI choice**

Default detector requires both readers/writers to be `*os.File` and uses `term.IsTerminal(int(file.Fd()))`. Tests inject the result.

Resolve UI mode after config load but before opening/creating a session. Forced non-TTY TUI returns: `otto: --ui tui requires terminal stdin and stdout; use --ui repl for redirected input`.

Add `OTTO_UI` to injected environment collection. Construct one controller, defer close, then call TUI or REPL. REPL retains existing OS interrupt routing. TUI uses Bubble Tea key handling and `WithContext`; external OS interrupt cancels process context, and `WithoutSignalHandler` prevents duplicate handlers.

- [ ] **Step 5: Run GREEN and regression gates**

```bash
gofmt -w internal/tui cmd/otto
go test ./internal/tui ./cmd/otto
go test -race ./internal/tui ./cmd/otto
go test ./...
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add internal/tui cmd/otto internal/config go.mod go.sum
git commit -m "feat: select adaptive terminal UI"
```

---

### Task 9: Add macOS Pseudo-Terminal Smoke Coverage

**Files:**
- Create: `cmd/otto/tui_pty_test.go`

**Interfaces:**
- Consumes: `github.com/creack/pty`, TUI runner, fake app backend.
- Produces: darwin-only integration proof for full-screen lifecycle.

- [ ] **Step 1: Add the PTY dependency and write the failing smoke test**

```bash
go get github.com/creack/pty@v1.1.24
```

Use `//go:build darwin`. Open a PTY pair, set 100x30 size, run `tui.Run` with the slave, and collect master output concurrently. The fake backend blocks Prompt until cancellation and emits a text delta first.

Test sequence:

1. Wait for alternate-screen enter bytes.
2. Send prompt plus Enter.
3. Wait for streamed assistant text.
4. Resize PTY to 80x24.
5. Send Escape and verify backend context cancellation.
6. Send Ctrl+C twice and wait for clean return.
7. Assert alternate-screen exit bytes were written.

Use deadlines on every wait and close both PTY descriptors in cleanup.

- [ ] **Step 2: Run RED**

```bash
go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=1
```

Expected: FAIL until test harness exposes any missing program behavior; if it passes immediately, temporarily remove `view.AltScreen = true` to prove the assertion fails, then restore before proceeding.

- [ ] **Step 3: Make the minimal integration corrections**

Correct only real issues surfaced by the PTY: flush/wait ordering, context shutdown, resize handling, or terminal restoration. Do not add features.

- [ ] **Step 4: Run GREEN and repeated race checks**

```bash
go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=5
go test -race ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=3
go test ./...
```

Expected: PASS without timeout or leaked goroutine.

- [ ] **Step 5: Commit**

```bash
git add cmd/otto/tui_pty_test.go internal/tui go.mod go.sum
git commit -m "test: cover TUI terminal lifecycle"
```

---

### Task 10: Document and Verify the TUI Milestone

**Files:**
- Modify: `README.md`
- Modify: `AGENTS.md`

**Interfaces:**
- Produces: accurate Stage 1 TUI usage and contributor verification instructions.

- [ ] **Step 1: Update README against real flags**

Document:

```text
otto --ui auto
otto --ui tui
otto --ui repl
OTTO_UI=repl otto
```

Add adaptive selection, full-screen alternate-buffer behavior, keybinding table, commands, Markdown/tool rendering, non-TTY fallback, terminal-size behavior, and the existing unsandboxed-shell warning. Keep Codex/Claude under planned providers only.

- [ ] **Step 2: Update AGENTS package boundaries and commands**

Add `internal/app` and `internal/tui` responsibilities. Add the PTY test command and state that tests must not require a real provider or interactive terminal.

- [ ] **Step 3: Run complete verification**

```bash
test -z "$(gofmt -l .)"
go test ./...
go test -race ./...
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
go build -trimpath -o ./otto ./cmd/otto
./otto --help
printf '/exit\n' | OTTO_API_KEY=test ./otto --ui auto --provider openai-compatible --model test --base-url http://127.0.0.1:1/v1 --no-session
git diff --check
rm ./otto
```

Expected: all gates exit zero; piped `auto` selects REPL and does not emit alternate-screen control sequences.

- [ ] **Step 4: Inspect dependency and repository state**

```bash
go mod tidy
git diff --check
git status --short --branch
git log --oneline --decorate main..HEAD
```

Expected: only intentional source/docs/dependency changes; no `.superpowers` artifacts; original-checkout `hello.py` remains outside this worktree and untouched.

- [ ] **Step 5: Commit**

```bash
git add README.md AGENTS.md go.mod go.sum
git commit -m "docs: document adaptive Charmbracelet TUI"
```
