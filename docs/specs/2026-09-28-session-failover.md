# Session failover

Status: approved 2026-09-28. Step 1 (durable notifications and the local
part of the commit rule) is implemented; steps 2 and 3 are not.

## A session cannot continue on another host today

Facts from the code at `b965a94`, before step 1:

- Each session record is written and `fsync`ed before the append returns
  (`write_pi_record`, `crates/otto/src/session/store.rs:760`). The assistant
  message that carries tool calls is appended before the tools run
  (`crates/otto-core/src/agent/mod.rs:362`).
- Tool calls run one at a time. Each result is appended before the next call
  starts (`agent/mod.rs:377`, append at `:430`). A failed append ends the
  turn. When the turn is cancelled, the running call returns its tool's
  cancellation result and every later call in the message gets a `cancelled`
  error result without running (`:387`). In every assistant message, the
  calls with results are therefore a prefix of its calls.
- On open, an incomplete final line is truncated and every tool call without
  a result gets the same synthetic error result (`repair_dangling_tool_calls`,
  `store.rs:500`; text `MISSING_TOOL_RESULT_TEXT`,
  `crates/otto-core/src/session/context.rs:1164`).
- `write` and `edit` replace a file with `write_file_atomic`
  (`crates/otto/src/tool/write.rs:103`): temporary file in the destination
  directory, `fsync`, `rename`. The directory is not `fsync`ed after the
  `rename` (`:139`). `bash` writes go through the page cache and are not
  synced by Otto.
- Mutual exclusion is `flock(LOCK_EX|LOCK_NB)` on the session file
  (`crates/otto/src/session/fsops.rs:75`). Whether it excludes a process on
  another host depends on the file system.
- `Store::open` seeks to the end once and then writes at the file position
  (`store.rs:186`); the file is not opened with `O_APPEND`. Two processes on
  two hosts writing one session file overwrite each other's bytes.
- The notification inbox (sub-agent results and reports, parent messages,
  timer firings) is in memory only (`crates/otto-core/src/agent/inbox.rs`).
  `deliver_notifications` drains it and then appends each item
  (`agent/mod.rs:456`); a failed append loses the drained items.
- A timer is removed from `<id>.reminders.json` before its notification is
  pushed into the in-memory inbox (`Inner::fire`,
  `crates/otto/src/tool/remind.rs:133`). An exit between the two, or before
  the next delivery, loses the timer.
- Sub-agent task ids come from a per-process counter
  (`crates/otto/src/subagent/tasks.rs:273`) and restart at `t1` after every
  resume. Child transcripts are `<id>/<task id>-<random id>.jsonl`
  (`crates/otto/src/cli/wiring.rs:1227`). A child transcript does not record
  whether its task finished.
- Reopening a session starts no turn. A wake turn starts only when the inbox
  is non-empty (`Controller::prepare_wake`, `crates/otto/src/app/mod.rs:999`).
- `SIGTERM` cancels `otto serve` (`crates/otto/src/cli/serve.rs:974`). The
  REPL, the TUI, and `--prompt` install no `SIGTERM` handler, so the process
  exits at once.
- The five SQLite stores open in WAL mode (`usage.rs:198`, `workflow.rs:533`,
  `memory/sqlite/mod.rs:293`, `skill/check.rs:536`,
  `subagent/record.rs:412`). SQLite documents that WAL requires every
  connection to be on one host and does not work over a network file system.
- The state directory is `$HOME/.otto`, with no override
  (`crates/otto/src/cli/run.rs:407`).

## Decisions made before this document

1. Target: continue a session on another host or container, both after an
   unplanned loss of the host and on request.
2. Durability of bytes is the distributed file system's job. Otto does not
   replicate data.
3. Otto enforces a single writer per session itself, not the scheduler.
4. A new host continues from the latest consistent state: the session log
   up to its last complete record, and the workspace as the distributed file
   system holds it. There are no snapshots and no return to an earlier
   point.
5. A tool call is committed when its result is in the log. A consistent
   state requires Otto to make a call's workspace effects durable before it
   appends the result (next section). A call without a result is reported to
   the model on the next host. Its effects are not undone; the model checks
   and corrects them.
6. An interrupted sub-agent continues only when the parent agent asks for it.
   No person confirms it.
7. Whether a session uses the lease is recorded with the session, not taken
   from the configuration of the host that opens it.

## Scope: one session, its sidecar files, and its workspace

In scope: one file-backed top-level session and its sub-agents, the session's
sidecar files, and the workspace directory.

Not in scope:

- Undoing the effects of a call without a result, including partial `bash`
  effects and effects outside the workspace.
- Snapshots of the workspace or the session, and returning to an earlier
  point.
- Resuming an in-flight provider response or tool call. Model output is not
  reproducible, so the next host continues with new model output.
- Continuing an interrupted sub-agent without a request from the parent agent.
- Workflow runs (`workflows.db`), memory, usage, and task history. These stay
  on the host that wrote them.
- Two hosts writing one session at the same time.
- A command that turns a lease-managed session back into a plain one.

## A result is appended only after the call's effects are durable

This is the commit rule. With it, every call that has a result in the log
has its workspace effects on stable storage, and the log never records a
call as finished whose file changes a new host cannot read.

- `write_file_atomic` `fsync`s the destination directory after the `rename`
  (`tool/write.rs:139`), so a `write` or `edit` is durable when the tool
  returns. This applies with failover disabled as well.
- For a lease-managed session on Linux, `Registry::execute`
  (`crates/otto/src/tool/registry.rs:60`) calls `syncfs` on the workspace
  root after every tool call returns and before the result goes back to the
  agent loop. The parent agent and every child's `ChildTools`
  (`crates/otto/src/subagent/runner.rs:255`) run tools through
  `Registry::execute`, so this covers `bash`, MCP tools, and sub-agents. A
  `syncfs` failure turns the call's result into an error that says the
  call's effects may not be durable. `syncfs` is in `crates/otto`, not in
  `otto-core`, which must stay buildable for wasm.
- macOS has no `syncfs`; `sync(2)` may return before the data is written.
  On macOS the rule covers `write` and `edit` only (see limits).
- Whether `syncfs` on an NFS or CephFS mount makes client-cached data
  durable on the server has not been checked. The cost of one `syncfs` per
  tool call on these mounts has not been measured.

After a loss of the host, each agent has at most one call whose outcome is
unknown: the first call without a result in its last assistant message. The
calls after it in that message never started. The open-time repair gives the
two kinds different synthetic result text:

- first call without a result: the call may have run, fully or partly, when
  the previous host stopped. A `write` or `edit` left the file either
  unchanged or fully replaced. A `bash` command may have left partial
  changes. An MCP call may have changed state outside the workspace. Check
  the effects before repeating the call.
- later calls: the call was not executed.

## Hosts share the session directory and the workspace path

- `$HOME/.otto/sessions` is on the distributed file system on every host. The
  rest of `$HOME/.otto` (SQLite stores, ChatGPT credentials, config) stays on
  local disk: WAL does not work over a network file system, and the lease
  below covers only session files.
- The workspace is on the distributed file system and is mounted at the same
  absolute path on every host. The session header records the canonical
  workspace path, `--resume` rejects a different one
  (`session/prepared.rs:245`), and tool arguments and results in the log
  contain absolute paths.
- Every host has the same `config.toml`, MCP server definitions, and provider
  credentials (the API key environment variable for `openai-compatible`;
  `otto login` on each host for `chatgpt`).
- Linux has no sandbox driver; `bash` needs `--sandbox off` there.
- File-system requirements:
  1. data is durable when `fsync` returns;
  2. close-to-open consistency: a process that opens a file after another
     host's `fsync` returned reads that data;
  3. `open(O_CREAT|O_EXCL)` is atomic across hosts;
  4. `rename` within a directory is atomic.

  NFSv3, NFSv4, and CephFS provide all four according to their
  documentation; this has not been tested with Otto. Object-store FUSE mounts
  without atomic rename or exclusive create are not supported. Other services
  have not been checked.

## Otto enforces one writer per session with an epoch lease

### A session is lease-managed when `<id>.lease/` exists

Every open that takes the session file lock today (`Store::open`, resume,
archive) first checks for `<id>.lease/` beside the session file. When it
exists, the session is lease-managed: the open uses the lease protocol
below, and every rule in this document that applies to lease-managed
sessions applies, whatever the local `[failover]` setting is. A host whose
configuration disables failover therefore cannot write a lease-managed
session without holding its lease.

`[failover] enabled = true` in `config.toml` only decides whether an open of
a session without `<id>.lease/`, including the creation of a new session,
creates the directory. With `enabled = false` and no `<id>.lease/`, opening
a session behaves as it does today.

The lease duration L is also a property of the session. The host that
creates `<id>.lease/` writes its local `lease_seconds` (default 30) into it,
and every host uses that value; a different local `lease_seconds` has no
effect on an existing lease. One L on all hosts is required by the timing
condition below. The directory is created atomically:

1. Create `<id>.lease.tmp-<random>/`, write `lease.json`
   (`{"lease_seconds": L}`) into it, and `fsync` the file and the directory.
2. `rename` it to `<id>.lease`. If the rename fails because `<id>.lease`
   already exists and is not empty, another host created it first: remove
   the temporary directory and use the existing one.

### Files

`<id>.lease/` holds:

- `lease.json`: L, written once at creation and never changed.
- `epoch-<n>`: created with `O_CREAT|O_EXCL`. The process that creates
  `epoch-<n>` holds epoch n. Content: host name, pid, start time.
- `heartbeat`: one 512-byte record overwritten in place with `pwrite` and
  `fsync`. Content: epoch, a sequence number incremented on every write,
  host, pid, `released`.
- `fenced-<n>.jsonl`: the session log as it was when epoch n was taken over.

### Acquiring the lease

1. Read L from `lease.json`, then the highest `epoch-<n>` and the heartbeat.
   With no epoch file, create `epoch-1`.
2. If the heartbeat says epoch n is released, create `epoch-<n+1>`.
3. Otherwise read the heartbeat bytes once per second, opening the file for
   each read. If the bytes change within 7L/6 on this host's monotonic clock,
   the holder is running: fail and report its host and pid. If they stay
   identical for 7L/6, create `epoch-<n+1>`. With the default L this is a
   35 s wait.
4. `EEXIST` on the create means another process created that epoch first:
   fail.

Only durations on the acquiring host's monotonic clock are compared; no
timestamps from two hosts are compared. A partly written heartbeat record is
compared as bytes like any other content, so liveness needs no parsing.

### Holding the lease

- One renewal thread per process rewrites the heartbeat of every held lease
  every L/3. Before each write it reads the monotonic clock; when `fsync`
  returns, it stores that reading as the last renewal. It also checks that
  `epoch-<n+1>` does not exist.
- One watchdog thread per process (an OS thread, not a tokio task, so a
  blocked runtime does not delay it) checks every second. When the last
  renewal is older than 5L/6, or the renewal thread saw `epoch-<n+1>`, the
  process is fenced: it sends `SIGKILL` to every child process it started
  (tool process groups, which `sandbox/process.rs:126` already creates, and
  MCP stdio servers) and exits with a distinct status without writing.
- Before each log append and each tool call, the holder checks the same two
  conditions in memory.
- On a clean exit, the holder writes the heartbeat with `released: true` after
  its last log write.

### Timing condition

Let T0 be the moment the acquirer starts reading the heartbeat. No renewal
was applied between T0 and T0 + 7L/6, or the bytes would have changed. The
holder's last successful renewal therefore started no later than T0, so its
watchdog fences it by T0 + 5L/6, and the acquirer takes over no earlier than
T0 + 7L/6. The L/3 gap (10 s with the default) must cover the fence itself
(kill and exit), the clock-rate difference between the two hosts over 7L/6,
and the acquirer's one-second polling. A holder process that is suspended
(VM pause, `SIGSTOP`) for longer than the gap can resume and perform one
write before its watchdog runs; see the limits section.

### Takeover moves the old log aside

After taking epoch n+1 from an unreleased epoch n:

1. `rename(<id>.jsonl, <id>.lease/fenced-<n>.jsonl)`. If `<id>.jsonl` is
   missing because a previous acquirer stopped after this step, use the
   highest `fenced-*.jsonl`.
2. Copy its complete lines to a new temporary file, `fsync` it, rename it to
   `<id>.jsonl`, and `fsync` the directory.
3. Open the new file with `Store::open` (existing lock, repair, and synthetic
   tool results).

Writes from the fenced holder that reach the server after step 1, including
pages still in its kernel cache, are written to the renamed inode and do not
reach the new log. The copy is bounded by `MAX_SESSION_FILE_BYTES` (256 MiB,
`crates/otto-core/src/session/pi.rs:29`). A released epoch skips this
section.

## An unreleased lease leads to forward recovery

### Notifications are durable until they are in the log

This part applies with failover disabled as well.

- Each inbox item gets a sequence number. `Inbox` gains an optional
  persistence hook that receives the full item list, under the inbox lock,
  after every push, delivery, and removal, so the file order matches the list
  order. The native side writes it to `<id>.inbox.json` (temporary file,
  `fsync`, `rename`, `fsync` of the directory); `<id>.reminders.json` uses
  the same helper. An empty inbox removes the file. A write failure is
  ignored, as a reminder file write failure from a firing timer is today;
  the in-memory item stays queued. Only the top-level session's inbox is
  persisted. Opening the session loads the file back into the inbox before
  any producer holds it. A load that restores at least one item signals the
  task registry's update channel, so the REPL, the TUI, and `otto serve`
  start a wake turn as soon as they wait on that channel. Archiving removes
  `<id>.inbox.json`. Both sidecar writers check that `<id>.jsonl` still
  exists before writing, so a timer or a sub-agent that finishes after the
  archive moved the session file does not recreate a sidecar. The check and
  the write are not atomic: a push between the check and an archive that
  runs in that interval can leave a sidecar without a session file.
- `deliver_notifications` reads the queued items, appends each one, and
  removes it from the inbox only after its append returned. A failed append
  leaves the remaining items queued.
- `Inner::fire` pushes the timer notification before removing the timer from
  `<id>.reminders.json`, under one hold of the timer lock, so a `cancel`
  either stops the notification or reports the timer as unknown.
- A notification is therefore delivered at least once. An exit between a log
  append and the following inbox file write delivers that one notification
  twice.
- When a sub-agent task is created, the runner writes a `custom` entry with
  `customType: "otto.task_spec"` as the first entry of the child transcript:
  task id, name, description, model, context, and the definition as resolved
  at start (name, body, tool allowlist, write policy, write paths). This is
  the same definition snapshot durable workflows store
  (`run_with_definition`, `crates/otto/src/subagent/runner.rs:600`). A queued
  task therefore has a transcript before it runs.
- When a sub-agent task reaches a terminal state, the runner pushes the
  completion notification into the parent's inbox and then appends a
  `custom` entry with `customType: "otto.task_result"` (status, error) to
  the child transcript. `status` is the task status name (`succeeded`,
  `failed`, `canceled`; step 2 adds `interrupted`). In this order an exit
  between the two writes cannot lose the completion; the task is then also
  reported as interrupted (see limits). A failure to write either entry is
  ignored, as a task record write failure is today, and does not change the
  task's outcome or its notification. Context building already skips
  `custom` entries, so neither entry reaches the model or the transcript
  views.
- The task id counter starts after the highest `t<k>` among the session's
  child transcript names, so a task id is unique within a session across
  resumes.
- The synthetic tool result text distinguishes the first call without a
  result in an assistant message from the later ones, as described in the
  commit rule section.

### The recovery notification starts a wake turn

After a takeover from an unreleased epoch, Otto pushes one notification into
the durable inbox. The push signals the task registry's update channel. The
REPL, the TUI, and `otto serve` start a wake turn on that signal, also when
it was sent before they began waiting on the channel.
`otto --resume <id> --prompt <text>` delivers it at the start of the prompt
turn. The text lists:

- the fenced epoch's host and pid;
- every tool call that the open-time repair gave a synthetic result, marked
  as "may have run" or "not executed" by the rule above: tool name and
  arguments, each truncated to 2 KiB;
- every interrupted sub-agent task: a child transcript that has an
  `otto.task_spec` entry and whose last entry is not an `otto.task_result`
  entry. Transcripts written before this change have no `otto.task_spec`
  entry and are not listed. For each: task id, name, agent, description, the
  first 500 bytes of the delegated prompt, whether it had started, its tool
  calls without results (marked the same way), and its last assistant text
  truncated to `max_output_bytes`;
- how to continue a task (next section), and that a task the parent does not
  continue stays interrupted.

Otto then appends `otto.task_result` with status `interrupted` to each listed
transcript, so a later recovery does not list it again unless it was
continued and interrupted again.

When nothing would be listed and the last parent log entry is an assistant
message without tool calls, the previous host was idle and no notification
is pushed.

### The parent agent decides whether an interrupted sub-agent continues

The `agent` tool (`crates/otto/src/subagent/tools.rs:74`) gains a `resume`
argument: the id or name of an interrupted task in this session. With
`resume`:

- `prompt` is required and is the message appended to the child transcript
  as the next user message. `agent`, `model`, `context`, `name`, and
  `description` must be absent; the task keeps the values in its
  `otto.task_spec` entry. `wait` works as for a new task.
- Otto opens the child transcript (the existing repair gives each call
  without a result its synthetic result), builds the child from the recorded
  definition rather than the current catalog, registers the task under its
  original id, and runs it through the same queue and `max_parallel` limit as
  a new task. Tools in the recorded allowlist that this host does not provide
  are left out, as at start (`allowed_tools`, `runner.rs:480`).
- A task that never started has only its `otto.task_spec` entry, so resuming
  it runs `prompt` as its first message.
- When the continued task ends, it pushes its completion notification into
  the parent's durable inbox and appends `otto.task_result`. The
  notification starts a wake turn through the existing path, and the parent
  agent finishes its own work.
- `resume` on a task that is not interrupted, or not in this session, is a
  tool error.

No person is asked. The parent model decides from the recovery notification
which tasks to continue, what to tell each one about its calls without
results, and which tasks to leave interrupted.

## A planned migration is SIGTERM and cancels running work

When the process holds the lease of at least one session, the first
`SIGTERM` in the REPL, the TUI, `--prompt`, or `otto serve`:

1. Cancels every running turn and every sub-agent task, the same path as
   `SIGINT` today (`crates/otto/src/main.rs:44`) and `Tasks::close`
   (`crates/otto/src/subagent/tasks.rs:602`). Each agent loop appends a
   result for its running call (the tool's cancellation result) and a
   `cancelled` result for every later call, so no log ends with a call
   without a result.
2. Waits until every agent loop has returned.
3. Appends `otto.task_result` with status `interrupted` to the transcript of
   each task that was queued or running, and pushes one notification that
   says the session was moved and lists those tasks in the recovery format,
   so the parent on the next host can continue them with `resume`.
4. Closes MCP servers, writes the heartbeat of every held lease with
   `released: true`, and exits 0.

The next host takes epoch n+1 at once, without waiting and without moving
the log, and the queued notification starts a wake turn there. A running
tool call is cancelled, not waited for; its result says so, and the model
repeats it on the next host if needed.

A second `SIGTERM` exits at once without releasing the lease, which leads to
forward recovery on the next host. A scheduler must allow more time before
`SIGKILL` than cancellation takes; the Kubernetes default is 30 s. A
process that holds no lease handles `SIGTERM` as it does today.

## Limits Otto cannot close by itself

- Writes the fenced host had already handed to its kernel reach the file
  system later. The session log is protected by the rename at takeover.
  Workspace files, `<id>.inbox.json`, `<id>.reminders.json`, and child
  transcripts are not. Storage-side fencing closes this: NFSv4 revokes an
  expired client's state and rejects its writes; CephFS can evict and
  blocklist a client. Otto configures neither, and the exact behavior of each
  implementation has not been checked.
- The lease is correct only while the clock-rate difference between hosts
  and process suspension stay within the L/3 gap.
- Processes not started by Otto, and background processes a tool started
  that keep running after the call returns, are stopped neither by fencing
  nor by `SIGTERM`. Their writes after the call's `syncfs` are not covered by
  the commit rule.
- On macOS, `bash` and MCP effects of a call that has a result may be missing
  on the next host if they were still in the lost host's page cache. The
  recovery notification cannot detect this.
- Each agent has at most one call with an unknown outcome after a loss of the
  host. Its effects are not undone.
- A sub-agent that ended after pushing its completion but before its
  `otto.task_result` entry is reported to the parent both as finished and as
  interrupted.
- Messages sent with `agent_send` that a child had not received when the host
  stopped are lost. The child's inbox is not persisted; the parent can repeat
  them in the `resume` prompt.
- The "not executed" text assumes Otto wrote the assistant message and ran
  its calls one at a time. A log written by Pi may come from calls that ran
  in parallel, so a call marked "not executed" there may have run.
- A failed write of `<id>.inbox.json` is not reported. Until the next
  successful write, the file lacks the items pushed since the last one, and
  a loss of the host loses them.
- An Otto version without this change does not check for `<id>.lease/` and
  writes a lease-managed session with only the file lock. Every host must
  run a version that has the lease-managed rule.
- L cannot be changed after `<id>.lease/` is created.

## Three pull requests, durable notifications first

Each step is one pull request and includes its tests.

1. Durable notifications and the local part of the commit rule: inbox
   sequence numbers, persistence hook, deliver-then-remove, a wake turn for
   a loaded inbox, timer order, `otto.task_spec`, `otto.task_result`, task
   id counter, the two synthetic result texts, and the directory `fsync` in
   `write_file_atomic` and in the sidecar writes. Applies without failover.
   This pull request also replaces the test-first rule in `AGENTS.md` and
   `docs/development.md` with a requirement for tests that fail when the
   covered behavior breaks.
2. Lease, takeover, recovery notification, `agent` `resume`, `syncfs` after
   each tool call on Linux, the lease-managed rule, and the `[failover]`
   config.
3. `SIGTERM` migration.

Ownership: new `crates/otto/src/failover` (lease, takeover, recovery
notification). In `otto-core`: `agent/inbox.rs`, `agent/mod.rs` (delivery
order), and `session/context.rs` (synthetic result text). In `crates/otto`:
`session/store.rs`, `session/prepared.rs` (archive acquires the lease, moves
`<id>.lease/` with the session, removes `<id>.inbox.json`),
`tool/write.rs`, `tool/registry.rs`, `tool/remind.rs`, `subagent/tasks.rs`,
`subagent/runner.rs`, `subagent/tools.rs`, and `cli` (config, signals). The
task map in `AGENTS.md`, `docs/development.md`, and the user manual change
in the step that adds each behavior.

## Tests run offline

All tests use temporary directories. Two hosts are simulated by two processes
or by two lease instances with injected clocks.

- Commit rule: for a lease-managed session, `Registry::execute` calls the
  sync hook after the tool returns and before the result is returned; a sync
  failure turns the result into an error. The directory `fsync` in
  `write_file_atomic` has no failure test, because making `fsync` fail needs
  an injection point that would exist only for the test; the existing
  `write` and `edit` tests cover the success path.
- Synthetic results: an assistant message with three calls and one result
  gives the second call the "may have run" text and the third the "not
  executed" text.
- Lease-managed rule: with `[failover] enabled = false`, opening a session
  that has `<id>.lease/` acquires the lease and fails while another holder
  renews; opening a session without it creates no directory. With `enabled
  = true`, two concurrent opens of a new session leave one `<id>.lease/`
  with one `lease.json`. A host whose local `lease_seconds` differs uses the
  value in `lease.json`.
- Lease: a running holder makes acquisition fail; a stopped holder is taken
  over after 7L/6; a released holder is taken over at once; of two
  concurrent acquirers one gets `EEXIST`; identical partly written heartbeat
  bytes count as unchanged.
- Watchdog: an injected renewal failure fences the holder after 5L/6 and kills
  a running tool's process group.
- Takeover: a writer that keeps its descriptor after the rename does not
  change the new log.
- Inbox: order under concurrent push, removal, and delivery; the file matches
  the list after each change; a failed append leaves the item queued; a
  reopened session delivers a loaded item once; a non-empty load signals the
  update channel.
- Timer: the notification is queued before the timer leaves
  `<id>.reminders.json`.
- Task entries: a queued task's transcript starts with `otto.task_spec`; a
  succeeded, a failed, and a cancelled task each end with `otto.task_result`,
  appended after the notification is queued; the task id counter continues
  after the highest `t<k>`; context building ignores both entries.
- Recovery: a log with a dangling call, a child transcript without
  `otto.task_result`, and a queued notification produce the notification text
  and one wake turn (fake provider); a log whose last entry is a final
  assistant message produces no notification.
- Resume: a parent `agent` call with `resume` continues a child from its
  transcript with the recorded definition although the catalog changed; the
  child's completion reaches the parent inbox and starts a wake turn;
  `resume` on a finished task and `resume` with `model` set are tool errors;
  a never-started task runs `prompt` as its first message.
- `SIGTERM`: with a fake provider, a slow tool, and a running sub-agent, the
  process appends a result for every call, marks the task interrupted,
  queues the migration notification, writes `released: true`, and exits 0.

A manual acceptance run with two Linux containers on one NFSv4 export is
documented in the development guide. It is not part of `make check`.

## Approved decisions

1. A session is lease-managed when `<id>.lease/` exists, whatever the
   local configuration is. `[failover] enabled` only decides whether a
   session without it gets one when it is opened or created, and
   `lease_seconds` is written into the directory at creation and then fixed
   for that session.
2. Only `$HOME/.otto/sessions` and the workspace go on the distributed file
   system; the SQLite stores stay local and are not recovered.
3. No snapshots and no return to an earlier point. A call without a result
   is reported to the model with "may have run" or "not executed"; its
   effects are not undone.
4. Commit rule: a call's workspace effects are made durable before its
   result is appended. Directory `fsync` for `write`/`edit` everywhere;
   `syncfs` after every tool call of a lease-managed session on Linux;
   macOS covers `write`/`edit` only.
5. Interrupted sub-agents are listed to the parent agent, which continues a
   task on its own transcript with `agent` `resume`; nothing continues
   without that call, and no person confirms.
6. `SIGTERM` in a process that holds a lease cancels running tool calls and
   sub-agents, records the tasks as interrupted, and releases the lease; it
   does not wait for running tool calls to finish.
