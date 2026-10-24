# Interactive use

Previous: [Getting started](getting-started.md) · [Documentation index](../README.md) · Next: [Safety and tools](safety-and-tools.md)

## Frontends

Selection:

- `auto` starts the interactive TUI only when **both** stdin and stdout are
  terminals. It falls back to the REPL for piped input, redirected output, and
  other non-TTY runs.
- `tui` forces the TUI and fails fast if stdin or stdout is not a terminal.
- `repl` forces the line-oriented REPL even from an interactive terminal.

```bash
otto --ui auto
otto --ui tui
otto --ui repl
OTTO_UI=repl otto
```

When stdin and stdout are terminals, the local REPL, one-shot prompt, TUI and
`--attach` TUI report Otto's root lifecycle through the terminal-native
[OSC 7501 Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status).
A supporting terminal can use these reports for tab indicators, notifications
or an agent inbox:

- before work starts, or after a canceled turn, the state is `idle`;
- an active turn is `working`;
- a pending human approval is `blocked` with `kind=permission`;
- a successful turn is `done`, and a failed turn is `error`.

`done` is intentionally retained while Otto waits for the next input, so a
supporting terminal can keep an unseen result in its inbox. Starting another
turn replaces it with `working`.

Reports use `app=otto` and fixed, non-sensitive messages; prompts, commands,
approval details and model output are never included. Otto sends reports
without a feature-detection query because terminals are required to ignore
unknown OSC sequences. Redirected and piped output receives no status escape
sequences. This integration currently reports only the root Otto process: it
does not report per-sub-agent records, progress percentages or a terminfo
capability.

### TUI behavior

- Feature panels open over the conversation. `Tab`/`Shift+Tab` change focus,
  arrows navigate, Enter activates, and Esc returns/dismisses without deciding.
  Returning from details restores selection/draft; queries do not block input.
- Approval panels select **Approve/Accept** on the left by default; use
  `←`/`→` and Enter to approve or deny. The read-only document does not take
  focus, but arrows/Page keys scroll it. There is no feedback field. Successful
  decisions close the panel and advance to the next request in creation order;
  errors keep it open. Esc leaves a request pending; `/approve` reopens it.
- Live approval notices in local and attached TUI sessions show labeled title,
  state, application status, reason/feedback and request details, omitting empty
  fields. Updates replace the same approval's display instead of repeating the
  request. Commands and code remain literal, with multiline JSON request values
  decoded once; terminal control sequences remain visibly escaped. This is enabled
  by default, with no extra configuration or permissions. Same-session history
  refreshes retain notices observed by this TUI, but restarting/resuming a session
  does not reconstruct structured notices from stored messages; approval discovery
  and decisions are unchanged.
- Runs inline and preserves the terminal scrollback; completed transcript rows remain
  available through normal terminal scrolling.
- Transcript markers distinguish submitted prompts (`❯`), replies/tool calls
  (`⏺`), indented results (`⎿`) and system notices/checkpoints (`✻`). Failed
  tools are red; prompts are shaded. Normal terminal scrollback remains usable.
- While a turn is running, an animated status line is shown under the
  transcript, and the composer title reads `Working — Enter queues for this
  turn · Esc cancels turn`. Once input is queued, a yellow `◆ QUEUED …` banner
  on a shaded background stays pinned immediately above the composer, even
  while the transcript scrolls. All accepted pending guidance also appears in
  the main conversation as temporary `[Queued for current turn]` user rows;
  queued-next input uses `[Queued next input]`. These rows are not written to
  history: delivery replaces a pending row with the actual user message rather
  than duplicating it, and withdrawal removes it. Startup, session switching
  and attached reconnect reload the current turn's pending guidance; a local
  queued-next input stays scoped to its original session. Local and attached
  terminals periodically refresh current-session guidance, including full text
  submitted by other clients. A delivered pending row remains visible until its
  actual user-message row arrives. Sending your own text returns the transcript
  to follow-bottom. Other clients' messages do not move a scrolled historical
  viewport: `NEW MESSAGES BELOW` above the composer offers `Ctrl+End` to jump
  to the bottom (with no feature screen open). Ordinary guidance uses
  `◆ QUEUED FOR CURRENT TURN` and changes the title to `Queued for current turn · Ctrl+U recall to
  edit · Esc delivers now`. `Ctrl+U` waits for the runtime to confirm that the
  guidance was not delivered, then returns its exact text to an empty composer
  for editing; a draft typed while recall is pending is never overwritten. A
  queued slash command uses `◆ QUEUED NEXT INPUT` and the
  title `Queued next input · Ctrl+U withdraw`, because it remains local until
  the active turn succeeds. Ordinary input is
  delivered within the same turn after the current provider response or tool
  call finishes. `Esc` while guidance is queued delivers it sooner: the
  running provider response or tool call is stopped, every queued message is
  appended, and the same turn continues with the next model call. A stopped
  tool call is recorded as `cancelled`; the tool calls after it in the same
  response are not started. Reply text streamed by a stopped model call stays
  on screen but is not saved in history, and the transcript shows `Step
  interrupted; delivering N queued message(s).` While an approval is pending,
  `Esc` does neither; the transcript names `Ctrl+C`, which cancels the turn.
  Sub-agent delegation never adds a blocking wait tool; child
  results arrive through a runtime-owned wake turn. Slash commands still wait
  until the turn finishes. If the turn ends before that delivery — `Ctrl+C`,
  `Esc` with nothing queued, a provider failure, or the turn deadline — the
  guidance is not delivered and Otto says so, quoting the message; when the
  composer was empty, the text is put back there to send again. The status
  line reads
  an animated spinner and the current phase, without thinking/reasoning or
  total-turn duration. The phase is `waiting for model`,
  `reasoning`, `responding`, `compacting`, or `running TOOL` (raw arguments
  stay in the expanded record). During a provider transport retry — a ChatGPT
  retry, or an OpenAI-compatible one whose reason is `connection failed`,
  `connection interrupted`, `model output idle timeout`, `HTTP 429` or
  `HTTP 503` — it shows
  `retry A/M after REASON, waiting Ns`, with a live countdown, followed by
  `requesting` once the delay ends. `A/M` counts retries (1/3 through 3/3);
  the retry countdown keeps updating. Esc or Ctrl+C cancels the retry,
  including its wait. This countdown is a retry delay, not reasoning duration.
- When a thinking effort is set and the provider returns reasoning summaries,
  the summary text streams as a dimmed `reasoning` entry before the reply and
  is saved in the session, so a resumed session shows it again.
- While idle, a pending notification — a finished sub-agent, or a `remind`
  timer — starts a wake turn that delivers it and lets the model continue.
- A due [`schedule`](safety-and-tools.md#schedule) item created from a terminal is shown as a
  system line and rings the terminal bell. When several TUIs or REPLs run on
  the host, including `otto --attach`, one of them shows each item. The REPL
  prints it as a line while it waits for input.
  Esc cancels it the same way as a user turn. Composer text is left in place.
- Assistant responses render as Markdown; if rendering fails, Otto falls back
  to escaped plain text.
- Tool calls default to the action/tool name, a known target (when available),
  explicit `running` / `completed` / `error` / `cancelled` status, and a bounded
  key result. Locally observed executions show live elapsed seconds, frozen at
  the tool's terminal result, including errors and cancellation. Retried attempts
  restart the execution clock within the same logical invocation. Timing is
  process-local, not inferred from history timestamps: resumed records and
  attached/replayed streams without trustworthy execution timing omit duration.
  Completed durations survive a history refresh within the same process/session.
  A stable 12-hex invocation fingerprint keeps calls distinguishable without
  crowding the summary (deduplication uses the full identity, not this label).
  Raw arguments, shell commands, output and file diffs are hidden by default;
  failures retain the first actionable error line and uncertain effects stay
  visibly `effect unknown`. Unknown tools fall back to their tool name.
  `Ctrl+O` shows full recorded arguments/output and colored file diffs, plus
  the full invocation ID and original provider call ID; it toggles back.
  Operation IDs keep live/replayed calls deduplicated across attach/reconnect;
  retries update the same logical invocation. Legacy records without operation
  identity use deterministic message/block IDs and cannot promise live replay
  deduplication. No configuration or migration is required, including on resume.
- The composer keeps a prompt history: `↑`/`↓` walk back and forth through the
  lines already submitted, and `↓` past the newest one restores what was being
  typed. It starts from the prompts the session already had, so a resumed
  session can recall its own, and switching sessions with `/resume`, `/fork`
  or `/new` replaces it with the new session's prompts. It is not persisted
  across runs.
- The composer shows at least 3 input rows even when empty or short, and
  grows with wrapped input up to 12 rows before it scrolls instead of
  growing further.
- Mouse wheel scrolls; left-drag copies visible transcript text on release,
  without gutters/padding but preserving code indentation. Any key/wheel clears
  the highlight. Clipboard support requires a host helper.
- The task panel shows up to four rows for this session's queued/running
  children; `/agents` includes other sessions and completed tasks.
- The footer shows model, approval mode, token totals/cache share, context,
  sandbox, effort, workspace and session ID; narrow terminals clip the tail.
  The totals are the whole session's: a sub-agent's tokens are added to them
  when its task finishes, and they survive resume, `--attach` and forks.
  Cached share is cached/input tokens and is omitted if any report, including a
  sub-agent's, lacks its breakdown. In `GET /v1/usage`, absent cache breakdown
  means unknown (`null`), not zero. Provider operations and actual HTTP attempts
  are counted separately.
- If the terminal is smaller than `40x8`, Otto shows a resize message.

### TUI keys

| Key | Action |
| --- | --- |
| `Enter` | Submit the current prompt, or run the highlighted slash-command suggestion |
| `Tab` / `Shift+Tab` | Complete a composer suggestion, or move focus within a feature screen |
| `↑` / `↓` | Recall the previous or next submitted prompt, or select a slash-command suggestion |
| `Shift+Enter` / `Alt+Enter` | Insert a newline in the composer |
| `?` | Open the help overlay when the composer is empty |
| `Ctrl+O` | Toggle a tool call between its folded summary and full arguments/output |
| Left-button drag | Select visible text and copy it to the clipboard on release |
| Mouse wheel, `PgUp` / `PgDn` | Navigate the focused screen control, or scroll the transcript |
| `Home` / `End` | Move the cursor to the start or end of the composer |
| `Ctrl+End` | Jump to the transcript bottom and resume following new messages (with no feature screen open) |
| `Ctrl+A` / `Ctrl+E` | Move the cursor to the start or end of the current composer line |
| `Esc` | Return or dismiss the active screen; at the composer, deliver queued guidance now, or cancel the active turn when nothing is queued |
| `Ctrl+C` | Cancel the active turn; when idle, a second press within one second clears and quits |

### The TUI attached to `otto serve`

```bash
otto --attach [--socket PATH] [--resume ID | --continue]
```

`otto --attach` runs the TUI as a client of a running `otto serve`. Sessions,
models, and tools run in the serve process, so one session can be open in
this terminal, in the web UI, in other `otto --attach` terminals, and in
`otto acp --attach` (the process `otto-connect` starts for chats) at the same
time. The attached process runs no model or tool and reads no provider
credentials. The socket defaults to `[server].socket`, then
`~/.otto/otto.sock`; TCP is not supported. stdin and stdout must be
terminals.

- Without a session flag, it opens a new session in the `--cwd` workspace,
  which `otto serve` must admit (see [Workspaces](servers-and-connectors.md#workspaces)).
- `--continue` opens the newest session of that workspace, or a new one when
  the workspace has none.
- `--resume ID` opens the session with that id, as `/session` and the web UI
  show it.
- `--attach` cannot be combined with `serve`, `--prompt`, `--no-session`,
  `--archive`, or `--ui repl`.

At startup it requests `GET /healthz` and opens the session. If either
fails, it prints `otto serve is not reachable at <path>: <error>` to stderr
and exits with status `1`.

Turns:

- A prompt is sent with `POST .../turns` and `"queue": true`, so it waits in
  serve's [turn queue](servers-and-connectors.md#turn-queue) while another client's turn runs. The
  prompt appears in the transcript when serve's `user_message` frame for the
  turn arrives. Terminal prompts contain text only.
- Ordinary text submitted while a turn runs steers that active task. Otto
  accepts it for the next safe provider boundary without interrupting a tool or
  silently creating a later turn. The pinned banner above the composer then
  shows `◆ QUEUED FOR CURRENT TURN …`. `Esc` then delivers the guidance now
  through `POST /v1/sessions/{id}/tasks/steer/interrupt`: serve stops the
  running provider or tool call and continues the same turn with the guidance
  appended. `Ctrl+U` recalls undelivered guidance
  into an empty composer for editing after the runtime confirms withdrawal.
  Guidance that wins the race and is
  delivered is not restored, and a new draft is never overwritten. Delivered
  guidance is session history and cannot be withdrawn. The guidance state is
  also available over HTTP (see [HTTP API](servers-and-connectors.md#http-api))
  as `GET /v1/sessions/{id}/tasks/steer`. An
  interrupted turn that never delivered it is
  reported with the text. If the turn has just ended, the text becomes
  a new turn; when an approval is pending, resolve that approval first. Use an
  explicit `/queue` command when the text is a separate follow-up task. Slash
  commands submitted while a turn runs retain their command-specific behavior.
- A turn started by another client of the session (the web UI, a chat, or
  another terminal) is shown while it runs: the transcript is reloaded up to
  that turn, then its prompt and events follow. A turn that started and
  finished between two status updates is shown by reloading the history.
- `Esc` with no guidance queued, and `Ctrl+C` at any time, cancel the turn
  being shown, including a turn another client started.
- An elevated `bash` command opens the approval dialog when serve reports it
  (see [Approvals inside a turn](servers-and-connectors.md#approvals-inside-a-turn)). **Allow** allows
  it and **Deny** denies it. `Esc` dismisses without deciding. If another client decides first, the dialog
  closes and the transcript shows `Approval <id> decided elsewhere:
  <decision>`.
- The footer, `/session`, and `/sandbox` show the serve process's session,
  model, and sandbox. The sub-agent panel shows the session's tasks from
  serve, and `/agents` reads serve's task records.

Commands available while attached: `/help`, `/init`, `/session`, `/new`,
`/clear`, `/resume` (the sessions of the workspace), `/fork`, `/rename`, `/compact`,
`/context`, `/agents`, `/permissions`, `/sandbox`, `/sandbox reload`,
`/approve [<id>]`, and `/exit`. Every other command prints
`/<command>: not available with --attach` and does nothing: `/model`,
`/thinking`, `/archive`, `/memory`, `/remember`, `/skill`,
`/tasks`, `/task`, `/timers`, `/mcp`, `/login`, `/logout`,
`/sandbox allow|network|exclude`, and `/approve <id> always`.

If a request to serve fails to connect, or the status stream or a turn's
event stream ends without that turn's `turn_end` frame, the transcript shows
`disconnected from otto serve` and the TUI retries every second. Prompts,
approval answers, `/new`, `/resume`, `/compact`, and `/sandbox reload` sent
in that state are refused with the same line. When serve
answers again, the TUI opens the same session, reloads its history, shows
`reconnected to otto serve`, and follows a running turn again. A turn that
was running when the connection was lost keeps running in serve.

### REPL behavior

- One prompt per line, entered at a `❯ ` prompt — the same marker the TUI
  puts in front of a prompt in its transcript.
- `Ctrl+C` during an active provider call, tool run, or compaction cancels only
  that turn and returns to the prompt; `Ctrl+C` while idle exits with status 130.

### Herdr

[Herdr](https://herdr.dev) is a terminal workspace manager for coding agents.
When Herdr starts Otto in a pane, Otto reports its own lifecycle over the socket
Herdr injects through the environment, so the pane appears as an agent named
`otto` with the states Herdr's bundled agents have:

| Otto | Herdr state | What it means |
| --- | --- | --- |
| Otto starts | `idle` | the pane becomes the agent `otto` right away, before the first prompt |
| A turn is running | `working` | the model is working |
| A tool call waits for your decision | `blocked` | the sidebar marks the pane, and `herdr agent wait <pane> --until blocked` returns |
| The turn ended | `idle` | Otto waits for input |

- Start Otto in a pane the usual way: `herdr pane run <pane> otto`, or type
  `otto` in a split. Herdr does not detect Otto by itself, so
  `herdr agent start --kind otto` is not available; the pane command is the way
  in.
- Otto reports state and session only, so it does not name itself: address it
  by the pane it runs in, as in `herdr agent prompt <pane> ...` and
  `herdr agent wait <pane> --until blocked`. For a name, rename it in Herdr
  once (`herdr agent rename <pane> otto`); `herdr agent list` and the agent
  panel show the label `otto` either way.
- Otto reports the session file and the command that resumes it
  (`otto --resume <session file>`), which is what Herdr needs to restore the
  session after its server restarts. Otto itself never depends on that.
- Reporting happens only inside a Herdr pane, and only for the frontend that is
  the pane's agent: `otto serve`, `otto acp`, `otto --attach` and `--prompt`
  runs do not report.
- The TUI uses the terminal's alternate screen, so `herdr agent read` sees only
  the current viewport. Ask Otto to write a long answer to a file when you need
  all of it.

## Slash commands

In the TUI, typing `/` opens a filtered suggestion panel; `Enter` executes only
an exact command. In the REPL, type the command and press `Enter`.

Shared commands:

- `/help` shows command help.
- `/init` asks the agent to inspect the repository and create a concise root
  `AGENTS.md` contributor guide. If the file already exists, it is left unchanged.
- `/session` shows session details (ID, path, provider, model, thinking effort,
  and sandbox state, plus the session name once `/rename` has set one).
- `/new` closes the current session and starts a fresh one in the same process.
- `/clear` is an alias for `/new`: it starts a fresh session, so the transcript,
  context counter, and per-session usage reset together.
- `/fork` opens a picker of the current session's prompts that start a turn,
  newest first. `Enter` writes a new session with the history before that
  prompt's turn, switches to it, and puts the prompt in the editor without
  sending it. See [Sessions](interactive-use.md#sessions). Without `--attach` it is rejected
  while a turn is in flight; in `--no-session` mode it reports that session
  persistence is disabled.
- `/rename <name>` renames the current session. The new name is written as
  append-only session metadata and is shown in session lists. It is rejected
  while a turn is in flight.
- `/model` shows the current profile/model and effort. In the TUI, bare
  `/model` opens a profile picker followed by a reasoning-effort picker whose
  default selection is the current effective effort; `Enter` applies it for the
  process and `s` saves it to the profile. Resuming another session keeps
  the current selection, including reasoning effort; it does not restore that
  session's historical model settings. In the REPL, use
  `/model PROFILE --thinking LEVEL [--save]`.
- `/thinking [LEVEL] [--save]` shows or changes the current reasoning effort.
  Use `unset` or `default` to omit the provider reasoning-effort field.
- `/compact [focus]` creates a manual context checkpoint, or reports
  `[context] no-op` when nothing can be compacted.
- `/sandbox` shows the sandbox state now in effect. `/sandbox reload` re-reads
  `[sandbox]` from the config file and applies it to the running process,
  printing the new state. It is rejected while a turn is in flight, and a
  failed reload keeps the previous sandbox in place. `allow_env` and `driver`
  changes, and a sandbox that was unavailable at startup, still need a restart.
- `/sandbox allow <path>` adds one path to `read_paths` and reloads, so a
  command that the sandbox denied can read it. The path is resolved to an
  absolute, symlink-free path first and must exist; it is written to
  `[sandbox].read_paths` in the config file, so it also applies to later
  sessions. In the TUI the resolved path is loaded before a confirmation screen
  whose selected action is `Cancel`; applying it also asks Otto to retry the
  command that failed. The model can also request the same persistent read
  grant through Bash's `sandbox_read_path` argument. Otto shows an approval
  dialog in the TUI or an Allow/Deny card in Telegram and Feishu, including
  the resolved path and the fact that it will be saved. This grant is always a
  user decision, whatever the session approval mode. Approval updates
  `read_paths`, reloads the sandbox, and asks the model to retry the command
  inside the sandbox. Denial or expiry leaves the configuration unchanged.
  This also works through `--attach`.
- `/sandbox network allow|deny` sets `[sandbox].network` the same way. In the
  TUI, `/sandbox network` without a mode opens a picker with the current mode
  marked.
- `/sandbox exclude <entry>` appends one entry to `[sandbox].excluded_commands`
  and reloads. The entry is validated first, and an entry already in the list is
  not repeated. In the TUI, this explicit command returns success or failure
  inline in the transcript, without opening a result dialog or requiring dismissal.
  Success identifies the saved rule and configuration file, confirms immediate
  reload, and asks Otto to retry a command blocked by the sandbox; failures show
  the validation or configuration error without asking the model to retry.
  The rule persists across restarts; no restart is needed to apply it. Other
  approval requirements still apply to matching commands. This command is not
  available with `--attach`. Removing an entry has no command: edit the config
  file and run `/sandbox reload`.
- If any of these changes is written but the sandbox rejects it, the configuration
  file is rolled back to what it held before and the previous sandbox stays in
  place.
- `/approve <id>` grants one pending elevated Bash command and immediately asks
  Otto to retry it. The grant is tied to the current session and exact command,
  is consumed once, and remains pending until it is replaced, consumed, or Otto exits.
- `/approve <id> always` first adds the pending command, exactly as approved,
  to `[sandbox].excluded_commands` (see [`[sandbox]`](getting-started.md#configuration)) and
  reloads the sandbox, then grants the pending command once as `/approve <id>`
  does. From then on the same command line, in this and later sessions, runs
  unconfined with no escalation approval; the same program with other
  arguments stays sandboxed. A command in a
  [classified class](safety-and-tools.md#classified-commands-wait-for-approval-inside-the-sandbox),
  such as `gh pr create`, still asks on each run. Otto refuses, writes nothing, and states the reason when:
  - the command is not simple (it chains, redirects or substitutes), or it
    contains `*`;
  - the first word is not a bare program name looked up on `PATH`: it contains
    a quote, a backslash, `=`, `/` or `~`. A path such as `./build.sh` or
    `/abs/dir/tool` can name a file in the workspace or in the sandbox's
    private home and temporary directories, which the model can rewrite;
  - the program runs another command given as an argument: shells and command
    runners such as `sh`, `bash`, `env`, `sudo`, `xargs`, `python3`, and
    `git`, which runs programs named by aliases (`alias.<name> = !...`) and by
    its configuration;
  - the program runs files from the working directory, so a fixed command line
    would still run whatever the model last wrote there: build and package
    tools such as `make`, `cargo`, `npm`, `go`, `pytest`, `docker`, and `awk`.

  Use `/approve <id>` for those. An exact entry still runs whatever files its
  arguments name: approve `always` only for a command whose arguments do not
  point at files the model can write. To exclude every invocation of a program
  deliberately, write `/sandbox exclude '<program> *'`. Entries of the form
  `<program> *` that earlier versions added are kept as they are.
  `/sandbox exclude 'git *'` is accepted, and it removes the protection
  described in
  [Git metadata is read-only](safety-and-tools.md#git-metadata-is-read-only-in-the-seatbelt-sandbox)
  for git commands.
- `/skill` opens the skill manager in the TUI; text frontends list the current
  session's available skills. `/skill <name>` displays one skill's description,
  location, contract-check status, and instructions, without executing it.
  `/skill set <name> enabled|disabled` saves its configured state. See
  [skills](memory-and-extensions.md#skills) for management and restart behavior.
  `/skills` is not a command.
- `/mcp` shows every configured MCP server and its connection state.
  `/mcp login <server>` signs in to one HTTP server that uses OAuth. See
  [MCP servers](memory-and-extensions.md#mcp-servers).
- `/login` runs the same ChatGPT sign-in flow as `otto login` without leaving
  the session, and reports `Restart Otto to use the new credentials`;
  `/login status` prints the sign-in state. On a non-`chatgpt` provider it
  answers that there is nothing to sign in to. `/logout` removes the stored
  credentials. See [ChatGPT subscription](getting-started.md#chatgpt-subscription).
- `/agents` lists sub-agent tasks recorded in `~/.otto/tasks.db` by every
  session of every Otto process on this machine, newest first, running or
  finished (see [Sub-agent task records](servers-and-connectors.md#sub-agent-task-records)). The REPL
  prints the latest 50 as one line each: parent session, task id, agent,
  status, created time, and the description (or the start of the prompt). The
  TUI opens a panel; see TUI-only commands. `/task` lists, inspects, or cancels
  tasks in only the current session; `/tasks` remains accepted as a compatibility
  alias for the list.
- `/exit` exits when idle (REPL EOF also exits).

TUI-only commands:

- `/context` opens a centered panel that lists what the next provider request contains:
  the system prompt parts (base, environment, workspace instructions, skills,
  agents), built-in and MCP tool definitions, the compaction summary, the
  previous turn's memory recall, and each message. The title shows the model,
  the estimated total against the context window, the last provider-reported
  input tokens, and the automatic compaction threshold. Token numbers are
  estimates (about 3 bytes per token), not tokenizer counts. `↑`/`↓` select,
  `Enter` expands a section or opens an item's full text (`↑`/`↓`/`PgUp`/`PgDn`
  scroll it), and `Esc` returns from details or closes the collection. The report is taken when the panel
  opens. The memory section is the previous turn's recall; the next turn
  recalls again with its own prompt.
- `/agents` opens a centered panel of recorded sub-agent tasks, newest first, with
  status, agent (`default` when none), description, workspace, parent
  session, created time, duration, steps, tool calls, and tokens. It starts
  with this workspace's tasks; `w` toggles between this workspace and all
  workspaces, and `s` cycles the status filter through all, queued, running,
  succeeded, failed, canceled, and interrupted. `↑`/`↓` or `PgUp`/`PgDn`
  select, `Enter` opens the task's prompt, result or error, and child
  transcript (`↑`/`↓`/`PgUp`/`PgDn` scroll it), and `Esc` goes back one
  level. The panel re-reads `tasks.db` every 2 seconds while it is open,
  including while a turn is running. `/agents` can also be opened while a
  turn is running (streaming, or waiting on a sub-agent): typing it and
  pressing `Enter` opens the panel instead of queuing it as the next prompt.
  `Esc` closes the panel (or leaves its detail pane) without cancelling the
  turn; a second `Esc`, with the panel closed, cancels the turn as it always
  does.

- `/resume` opens a centered panel of the up to 50 most recently modified valid sessions
  for the current canonical workspace. `↑`/`↓` or `PgUp`/`PgDn` to navigate,
  `Enter` to resume, `Esc` to close. It does not search other workspaces. Each
  session id appears once: files that share one (a copy beside the original,
  for example) collapse to the newest-modified file.
- `/archive` opens the same panel to archive a session. `Enter` on a non-current
  session moves it into `archive/` and shows `archived session <id>`. `Enter` on
  the current session archives it and starts a fresh session. `Esc` closes
  without archiving. It is accepted only while idle (a turn, `/new`, or
  `/resume` in progress is rejected) and reports `no active sessions found`
  when the workspace has none. In `--no-session` mode it reports that session
  persistence is disabled.

In the REPL, `/archive` archives the current session and starts a fresh one,
printing the archived path and the new session ID.

## Sessions

Otto writes append-only JSONL in the **Pi session format version 3**, compatible
with the public session format and `SessionManager` API in Pi 0.84.3. Otto
keeps its own storage root, separate from Pi:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

A new session file is created lazily: starting Otto reserves a session, but the
JSONL file is only written on the first user prompt. Starting and quitting
without a prompt leaves no session file behind. The first records are written
to `<session-id>.jsonl.<hex>.tmp`, synced, and then given the name
`<session-id>.jsonl`, so a process killed during that write leaves no
`<session-id>.jsonl`. It can leave the `.tmp` file, which Otto never reads and
which can be deleted.

```bash
./otto --cwd /path/to/project --continue
./otto --cwd /path/to/project --resume /absolute/path/to/session.jsonl
./otto --cwd /path/to/project --archive /absolute/path/to/active-session.jsonl
./otto --cwd /path/to/project --no-session
```

Notes:

- `--continue` reopens the newest valid Pi v3 session for the current canonical
  workspace. Invalid files and old Otto v1 files are skipped.
- `--resume PATH` reopens a specific valid Pi v3 session only when its recorded
  workspace matches the current `--cwd`.
- Resuming restores conversation history, not model selection. `/resume` keeps
  the running process's current profile, model and reasoning effort. Starting
  Otto with `--resume` or `--continue` uses the resolved startup configuration
  and command-line overrides, not the historical session's model settings.
  Historical model changes remain in the append-only session for tracing.
- Old Otto v1 files are left untouched, are not listed by `/resume`, and cannot
  be resumed.
- `--no-session` keeps history in memory only.
- **Archiving** moves an active session into a sibling `archive/` directory:
  `~/.otto/sessions/<workspace-key>/archive/<session-id>.jsonl`. The move is
  atomic and preserves the file byte-for-byte with its `0600` mode; nothing is
  deleted and no disk space is reclaimed. The `archive/` directory is created
  `0700` on the first archive and is scoped to that workspace, so archiving one
  workspace never affects another. Archived sessions are excluded from
  `/resume`, `--continue`, and the `/archive` picker, but remain resumable by
  explicit path:
  ```bash
  ./otto --cwd /path/to/project --resume ~/.otto/sessions/<key>/archive/<session-id>.jsonl
  ```
  `--archive PATH` archives one active session for the current `--cwd` and
  exits. It cannot be combined with `--continue`, `--resume`, `--no-session`,
  or `--prompt`.
- **Forking** writes a new session beside the current one whose history is the
  current session's history before a chosen prompt's turn, then switches to
  it with that prompt in the editor, to edit and send again. In the TUI,
  `/fork` lists the prompts that start a turn, newest first; `/fork <id>`
  forks before the prompt with that entry id. The fork keeps every earlier
  entry unchanged, compaction included, so the model sees the same context
  the source had at that point. The source session is never changed. Its
  queued inputs, reminders and sub-agent transcripts stay with it; earlier
  tool results that name a sub-agent task stay as text. A fork never ends
  inside a turn, so opening it starts no turn of its own. The fork's header
  records the source file in `parentSession` and its name is the source's
  name plus ` (fork)`; `/session` shows `Forked from: <id>`.
- **Sub-agent transcripts** are written beside the parent session, in a
  directory named after the parent file without `.jsonl`:
  `~/.otto/sessions/<workspace-key>/<session-id>/<task-id>-<child-id>.jsonl`.
  Each is a Pi v3 session whose header records the parent file in
  `parentSession`; it contains any inherited context, the delegated prompt,
  and every child message and tool call. The file is created on the child's
  first write, with the same `0700` directory and `0600` file modes. Child
  transcripts are not listed by `/resume` or `--continue`. Archiving a session
  moves its transcript directory into `archive/` with it. Under
  `--no-session`, children stay in memory. `/task <id|name>` prints the file
  as a `transcript:` line, and the task JSON carries it as `session_path`. If
  a child transcript cannot be created, only that task fails. A transcript
  starts with an `otto.task_spec` custom entry (task id, name, description,
  model, `context` (`fresh` or `inherit`), and the agent definition if one
  was used) and ends with an `otto.task_result` custom entry (`status`
  `succeeded`, `failed`, `canceled`, or `interrupted`, and the error text). Otto does not
  add these entries to the child's model context. Task ids continue from the
  highest `t<N>` in the transcript directory, so a resumed session does not
  reuse an id.
- **Undelivered notifications** (a finished or reporting sub-agent, a fired
  `remind` timer, a `[feishu]` message) are written to
  `<session-id>.inbox.json` beside the session file, mode `0600`, and removed
  from it once delivered to the model. Reopening the session restores them,
  and the REPL, the TUI, `otto serve`, and `otto acp` start a wake turn to
  deliver them.
  A turn started by user input delivers pending notifications before the
  user's message. Each notification has an input id, recorded on the message
  that delivers it, so one appended to the session just before Otto exits is
  not delivered again after the session is reopened. Archiving the session
  deletes the file. When the file cannot be written, `agent_send` and
  `agent_report` return an error and the message is not sent; a background
  notification (a finished task, a timer) stays queued in memory, and a
  timer whose notification was not saved fires again after a restart. An
  unreadable file is renamed to `<session-id>.inbox.json.corrupt-<seconds>`
  with a warning on standard error. `--no-session` keeps notifications in
  memory only.
- **Guidance for a running turn** (steering from the TUI, chat `/steer`,
  HTTP `/tasks/steer`, or ACP) is also written to `<session-id>.inputs.json`
  until it is delivered. Delivery, withdrawal, and every way a turn ends
  remove it. When the Otto process stops during the turn, the turn that
  resumes it after the session is reopened delivers that guidance first;
  the next turn that does not resume it discards it. Guidance that cannot be
  written is not accepted. Guidance a turn ends without delivering — the user
  cancelled it, the provider failed, the turn hit its deadline — is reported
  once, as a visible `agent_steering` context message and a notification
  quoting the message, so an interrupted turn never drops the user's words in
  silence.
- **Agent-scheduled follow-ups** (`task_queue`) are written to
  `<session-id>.followups.json` until their step runs; every way the logical
  task ends removes the rest. When the Otto process stops during the task,
  the wake turn that resumes it runs them afterwards in order, skipping a
  step whose message is already in the session. When the task is not
  resumed (its automatic resume was used, or Otto stopped between two
  steps), the next task discards them and stores a visible notice with
  their count.
- **Queued prompts** waiting for the active turn (`otto serve` turns sent
  with `"queue": true`, which includes the attached TUI and attached ACP, and
  local ACP `/queue`) are written to `<session-id>.turns.json` before they
  are accepted; a prompt that cannot be written is not queued. A prompt's
  entry is removed once it has run, or when it is cancelled, its session is
  deleted, or the session is archived. When the Otto process stops, or
  `otto serve` shuts down, before they run, the next frontend that opens the
  session (`otto serve`, local ACP `session/load`, or the TUI) runs them in
  their original order after the interrupted turn's automatic resume, each
  once: a prompt whose message is already in the session is not run again.
  The first of them delivers a notice saying how many were restored.
  Images in a queued prompt are not saved; the notice says when one was
  dropped. Opening nothing runs nothing. The REPL does not restore them;
  they wait for one of those frontends.
- **Unread parent messages** (`agent_send`) for a sub-agent task are written
  to `<child transcript>.inbox.json` beside the child's transcript. When the
  task is interrupted and later resumed with `agent` `resume`, they are
  delivered before the resume prompt, and the recovery notice counts them.
- Each turn writes an `otto.turn` start entry before anything else and an end
  entry (`completed`, `failed`, or `cancelled`) when it returns. The end entry
  of a failed turn also records the error that ended it, with known secrets
  redacted and truncated to 4 KiB. Neither entry reaches the model.
- When a session is reopened after its Otto process stopped in the middle of a
  turn (killed, out of memory, crashed, or the host lost power), Otto
  recovers it:
  - Tool calls in the last assistant message without results are run again
    when every one of them names `read`, `ls`, `grep`, `find`,
    `memory_search`, `agent_status`, `agent_yield`, `remind_status`, or `list_models`, the
    tool had the same read-only declaration when the call started, and the
    call was not already run again once. Otherwise the first such call is
    recorded as an error result saying the call may have run, and the calls
    after it as not executed.
  - Otto queues one notification saying the previous process stopped,
    listing the calls run again, the calls without results, and each
    sub-agent task that had not finished. Each listed task is recorded as
    `interrupted`, so the model can continue it with `agent` `resume`.
  - The notification starts a wake turn in the REPL, the TUI, `otto serve`,
    and `otto acp`, which continues the interrupted turn from the transcript.
    The resumed request is a new provider request; partial model output from
    before the stop is not kept. With `--prompt`, the notification is
    delivered before the prompt.
  - Otto resumes a turn on its own at most once. When the resumed turn is
    itself interrupted, the next open records error results for its calls
    without results, queues no notification, and prints
    `recovery: turn <id> was interrupted again after its automatic resume; not resuming`
    to standard error; the next prompt continues normally.
  - A turn that ended with an error or was cancelled is not resumed. Sessions
    written before `otto.turn` entries existed are recovered only when they
    end with calls without results. Nothing is resumed until a frontend opens
    the session; `otto serve` does not reopen sessions on start.
- Local manual and automatic compaction append Pi v3 `type: "compaction"`
  checkpoints carrying `firstKeptEntryId`, `tokensBefore`, optional usage, and
  bounded file metadata. Responses remote compaction instead appends an
  invisible `otto.remote_compaction` custom checkpoint. It is append-only and
  contains provider-owned encrypted context, so a resumed session can return
  it unchanged without exposing it as transcript text.
- Session files contain sensitive prompt text, responses, summaries, tool
  calls, tool arguments, results, and file metadata. Protect them like source
  data. They do not contain provider API keys, OAuth tokens, authorization
  headers, or cookie values, and Otto does not persist private sandbox profile
  paths as runtime metadata.

### Continuing a session on another host

A session whose directory `~/.otto/sessions/<workspace-key>/` is on a shared
file system can be continued by an Otto process on another host or container
after the first host is lost. Otto allows one writer per session through a
lease in `<session-id>.lease/` beside the session file.

- With `[failover] enabled = true`, a new session creates its lease directory
  on its first write, and opening an existing session without one creates it.
  Once `<session-id>.lease/` exists, every open, resume, and archive of that
  session uses the lease, on every host, whatever that host's `[failover]`
  setting is. There is no command that removes the lease from a session.
- Opening a lease-managed session that another process holds and renews fails
  with `session is held by host <host> pid <pid> (lease epoch <n>)`. When
  the holder exited
  cleanly, the open succeeds at once. When the holder stopped without
  releasing the lease, the open waits 7/6 of `lease_seconds` (35 s with the
  default) without output, then takes the session over.
- On a takeover Otto moves the old log to
  `<session-id>.lease/fenced-<n>.jsonl` and continues from a copy that holds
  only its complete records, so a write from the stopped host after that point
  does not reach the session. The session is then recovered as after a
  stopped process (see the session file description above): read-only calls
  without results are run again, other calls get error results, and one
  notification names the stopped host and pid, the calls without results
  (marked "may have run" or "not executed"), and each sub-agent task that had
  not finished, and starts a wake turn that continues the interrupted turn.
  A task that cannot be recorded as `interrupted` is reported as a warning on
  standard error. No notification is queued when there is nothing to report
  (no call without a result, no unfinished task, and the session ends on a
  finished assistant reply), or when sub-agents are disabled or no provider is
  configured.
- The model continues an interrupted task with the `agent` tool's `resume`
  argument (the task id or name) and a `prompt`. The task keeps its recorded
  agent definition, model, and context; `resume` on a task that is not
  interrupted is an error. Empty `agent`, `model`, `name`, and `description`
  values and `context: "fresh"` count as omitted; any other value for those
  arguments is an error. A task that is not continued stays interrupted.
  Effects of calls without results are not undone.
- `SIGTERM` to a process that holds a lease (the REPL, the TUI, `--prompt`, or
  `otto serve`) moves the session to the next host that opens it. Otto
  cancels every running turn, tool call, and sub-agent task, waits until each
  agent has written a result for every call, records each task that was
  queued or running as `interrupted`, queues one notification that says the
  session was moved and lists those tasks in the format above, closes MCP
  servers, marks the lease released, and exits `0`. The next open, on any
  host, takes the session at once, without the 7/6 wait and without moving
  the log, and the notification starts a wake turn there. A running tool call
  is cancelled, not waited for; its result says so. No notification is queued
  when no task was cancelled and the session ends on a finished assistant
  reply. A second `SIGTERM` exits at once without releasing the lease, so the
  next host takes the session over as after a lost host. A scheduler must
  allow more time before `SIGKILL` than the cancellation takes; the
  Kubernetes default is 30 s. A process that holds no lease handles `SIGTERM`
  as it did before: `otto serve` shuts down, and the REPL, the TUI, and
  `--prompt` are ended by the signal.
- A process that holds a lease renews it every third of `lease_seconds`. When
  renewal has not succeeded for 5/6 of `lease_seconds`, or another host has
  taken the session over, the process kills the processes its tools started
  and exits with status `75` without writing. Tool calls and log writes are
  refused once the lease is lost.
- On Linux, after each tool call of a lease-managed session Otto runs
  `syncfs(2)` on the workspace's file system before the result is written; a
  sync failure turns the result into an error. macOS has no such sync, so
  `bash` and MCP effects of a call with a result can be missing on the next
  host.
- Requirements and limits: the file system must make data durable when `fsync`
  returns, show another host's synced data to a later `open`, and provide
  atomic exclusive create and atomic rename. This has not been tested on any
  network file system. Writes the stopped host had already handed to its
  kernel can still reach the workspace, `<session-id>.inbox.json`,
  `<session-id>.reminders.json`, and sub-agent transcripts. The lease assumes
  clock-rate differences and process suspensions between hosts stay under a
  third of `lease_seconds`. Workflow runs, memory, usage, and task records
  stay on the host that wrote them. Every host must run an Otto version that
  checks for `<session-id>.lease/`; an older version writes the session with
  only the file lock.

### Optional Pi interoperability probe

If Pi 0.84.3 (or a compatible package exposing the public Pi v3 `SessionManager`
API) is installed, an opt-in probe opens one session and prints bounded JSON
metadata only — never message, summary, or tool content:

```bash
OTTO_PI_INTEROP=1 node ./scripts/pi-session-interop.mjs /tmp/otto-session.jsonl
```

It exits 77 with a `SKIP` message when Pi is unavailable or the gate is unset,
and exits nonzero for an invalid session.

## Context compaction

Compaction is configured in `[agent.compaction]` and, when needed, with
per-profile context metadata.

Rules and defaults:

- `auto = true` is the default. With `auto = true`, Otto does three bounded
  automatic checks:
  - **Proactive compaction:** when the model window is known and the next
    request estimate is above `working_window - reserve_tokens`, Otto attempts
    one checkpoint before sending the provider request.
  - **Idle compaction:** when the last model response is at least 60 minutes
    old (the provider prompt cache has likely expired) and the next request
    estimate is above half of `working_window - reserve_tokens`, Otto attempts
    one checkpoint before sending the request. It shares the one-shot attempt
    with proactive compaction.
  - **Reactive overflow recovery:** when the provider returns a typed
    context-overflow error, Otto does one automatic compaction and retries once.
- The automatic paths are one-shot only; they never loop.
- When proactive compaction fails and the estimate is below
  `hard_input_window - reserve_tokens`, Otto emits a `compaction_warning` that
  names the cause and sends the original request. At or above that limit the
  turn fails.
- A provider that explicitly advertises Responses remote compaction receives
  the current context plus a `compaction_trigger`. Otto stores the returned
  encrypted compaction item unchanged and sends it only to the same provider;
  it is never displayed, logged, or treated as readable summary text. A
  provider without that capability uses Otto's local structured summary. Once
  a provider advertises support, authentication, transport, cancellation and
  malformed-response failures are surfaced rather than hidden by a second
  local request.
- For local compaction, reasoning the model returns with a summary is
  discarded; only the summary text is stored in the checkpoint.
- `reserve_tokens = 16384` and `keep_recent_tokens = 20000` are the defaults.
- `context_window` and `compaction_window` must be at least `4096` when set.
- `compaction_window` requires `context_window` and must not exceed it.
- Manual `/compact [focus]` works in both frontends even when `auto = false`.

The optional `focus` text is sanitized, bounded to 8 KiB, and appended only to
the hidden summary-system prompt.

What a checkpoint keeps:

- The model-written summary has an `## Observations` section. The summary
  prompt asks it to keep observations needed to continue, with their source
  (tool call, command, or user statement) and result, and to record user
  corrections as `user: ...`. It asks the model to merge duplicates, replace
  superseded observations with supported current results, and omit resolved
  transient errors and obsolete attempts, including those in a previous
  summary. Failures remain when their cause or correction still matters.
  The full session history remains append-only. A summary without the section
  is rejected. Otto checks that the section is present, not semantic relevance.
- The text of every summarized user message is appended to the summary
  verbatim, one JSON string per line, in a `<user-messages>` block. Otto builds
  this block itself, so the summary model cannot change or drop its lines. The
  block carries forward across later compactions. When it would exceed 16 KiB,
  the oldest messages are dropped. A message longer than 2,000 characters is
  cut and marked `[user message truncated for compaction]`.
- The text of every summarized `capability_update` message is appended the same
  way in a `<capability-updates>` block after `<user-messages>`. When it would
  exceed 16 KiB, the oldest notices are dropped. A notice longer than 8,000
  characters is cut and marked `[capability update truncated for compaction]`.
- The paths the summarized tool calls read and modified are appended in
  `<read-files>` and `<modified-files>` blocks.

### Model window metadata

Otto ships a static limit catalog for common GPT, o-series, and Claude model IDs
so it can size compaction conservatively.

- Listed full-size GPT-5.4/5.5/5.6 aliases use `context_window=1050000`,
  `hard_input_window=922000`, and `compaction_window=272000`.
- `gpt-5.4-mini`, `gpt-5.4-nano`, GPT-5/5.1/5.2 aliases, and `gpt-5.3-codex` use
  the 400K catalog family with a 272K working window.
- `gpt-5.3-codex-spark` is an exact 128000-context / 32000-output override;
  `*-chat-latest` aliases use their catalog chat values.
- GPT-4.1, GPT-4o, o1/o3/o4, and listed Claude aliases use the static catalog.
- Claude metadata is used only when an OpenAI-compatible endpoint exposes a
  Claude-family model ID; Otto has no Anthropic provider.

If a model ID is unknown, proactive automation is disabled (no trustworthy local
window), but reactive one-shot recovery can still happen after a typed provider
overflow. For private deployments, set `context_window` and optionally
`compaction_window` on the selected profile.

## Headless mode

`--prompt` runs a single prompt without interaction and exits: `0` on success
or after a `SIGTERM` that moved a lease-managed session (see
[Continuing a session on another host](interactive-use.md#continuing-a-session-on-another-host)),
`1` on error, `130` on interrupt. The value is the prompt text, or `@PATH` to
read the prompt from a file (bounded to 1 MiB).

```bash
./otto --prompt "summarize TODOs in this repo"
./otto --prompt @prompt.txt --no-session
./otto --prompt "explain main.go" --thinking max --continue
```

`--prompt` cannot be combined with `--ui tui` or `--archive`, and composes with
`--continue`, `--resume`, and `--no-session`.

Every turn of a `--prompt` run, including the wake turns that deliver
background task results, stops after
[`[agent].unattended_turn_timeout`](getting-started.md#configuration)
(default `30m`) and the run exits with `1`. Set it to `"0"` to remove the
limit.

---

Previous: [Getting started](getting-started.md) · [Documentation index](../README.md) · Next: [Safety and tools](safety-and-tools.md)
