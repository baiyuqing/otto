# Limits and deadlines

Every `config.toml` key and built-in limit that stops or bounds agent work is
listed here with its meaning, default and effect. Deadlines are
Go-duration strings such as `"90s"`, `"30m"` or `"1h30m"`. Keys are in the
`[agent]` and `[agents]` tables of [config.toml](getting-started.md#configuration).
Restart long-lived processes (`otto serve`, chat connectors) after changing them.

## Summary

| Key | Bounds | Default | `0` | When hit |
| --- | --- | --- | --- | --- |
| `[agent].turn_timeout` | One user turn, in interactive use | none | rejected at load | The turn stops; the session continues |
| `[agent].unattended_turn_timeout` | One chat or `otto --prompt` turn | `30m` | disables the limit | The turn stops; `--prompt` exits with `1` |
| `[agent].provider_timeout` | One model call, including its retries | none | rejected at load | The request fails with `provider call deadline exceeded` |
| `[agent].cancellation_grace` | Wait for a cancelled operation to settle | `5s` | rejected at load | The operation is reported as stopped without waiting longer |
| `[agent].subagent_timeout` | One sub-agent task | none | rejected at load | The task ends as `interrupted` with `task deadline exceeded` |
| `[agent].workflow_step_timeout` | One workflow step attempt | none | rejected at load | The step becomes `interrupted` and the run `paused` |
| `[agent].shell_timeout` | One `bash` tool call | `120s` | rejected at load | The command is killed; the result reads `status: timed out after <duration>` |
| `[agent].max_output_bytes` | Text returned by one tool call | `51200` | uses the default | The text is cut and ends with `[truncated: <n> bytes omitted]` |
| `[agent].no_progress_repeats` | Identical consecutive tool calls, or unchanged cycles of 2-8 calls | `8` | disables this condition | The turn ends with a `no progress:` error |
| `[agent].no_progress_failures` | Consecutive failed tool results | `12` | disables this condition | The turn ends with a `no progress:` error |
| `[agent.budget].max_tokens` | Tokens one turn uses: input plus output over every model response, compaction responses included | `4200000` | disables this limit | The turn ends with a `budget exceeded:` error; the session continues |
| `[agent.budget].max_steps` | Model calls one turn makes, not counting compaction requests | `54` | disables this limit | The turn ends with a `budget exceeded:` error; the session continues |
| `[agents].max_parallel` | Concurrent sub-agent tasks; concurrent workflow steps | `4` | rejected at load | Further tasks or steps wait in a queue |
| Scheduled run limit (built in) | One scheduled run | `10m` | not configurable | The run is cancelled and recorded as failed |
| Workflow step count (built in) | Steps in one workflow definition | 32 | not configurable | The definition is rejected |

A value that is out of range fails at load with an error that names the key.

## Turn deadlines

`turn_timeout` applies to every turn except an unattended one, and also to
manual compaction. It is unset by default, so an interactive turn has no
deadline. When it expires the turn stops with the error
`provider call deadline exceeded`, also when the deadline passes during a tool
call. The session stays usable and the next prompt runs normally.

`unattended_turn_timeout` applies to a turn that comes from a chat connector
(origin `chat:<platform>:...`) and to every turn of an `otto --prompt` run,
including wake turns that deliver background task results; the run exits with `1`
when the limit stops a turn. When `turn_timeout` is
also set, an unattended turn uses the smaller of the two. Setting
`unattended_turn_timeout = "0"` removes the unattended limit; `turn_timeout`
still applies to such turns if it is set. Scheduled runs do not use this key; they
are bounded by the built-in scheduled run limit below.

## Model requests

`provider_timeout` bounds one model call, including the retries Otto makes
inside that call. It is unset by default, so a call is bounded only by the turn
deadline. A request that hits it fails with
`provider call deadline exceeded`; the agent then ends the turn with that error.
`cancellation_grace` is the time Otto waits for a request, tool call, MCP call or
shell process to settle after a cancellation or deadline before it reports the
operation as stopped; its effect on an external side effect that already started
is not known to Otto. Cancellation cannot prove that such a side effect did not
occur; see [recovery](troubleshooting.md) before retrying a write.

## Shell calls and tool output

`shell_timeout` is the limit for one `bash` call. When it expires the command is
cancelled and the tool result reads `status: timed out after 2m0s` (the duration
is the configured value). The turn continues and the model sees the result.
`max_output_bytes` caps the text one tool call returns to the model; the same
value caps skill, web and other text results. Omitting the key, or setting `0`,
gives `51200`.

## Sub-agents and workflows

Sub-agent and workflow deadlines are independent of the deadline of the turn that
started them:

- A sub-agent task has no parent deadline. It is bounded only by
  `subagent_timeout`, if set. A task that outlives the turn that started it keeps
  running after that turn ends; stop it with `agent_cancel` or
  `/task cancel <id|name>`.
- A task that hits `subagent_timeout` ends as `interrupted` with the error
  `task deadline exceeded`. It can be resumed explicitly; see
  [sub-agents](safety-and-tools.md#sub-agents).
- A workflow step attempt gets its own deadline from `workflow_step_timeout`. When
  it expires the attempt and step become `interrupted` with
  `workflow step deadline exceeded`, and the run becomes `paused`. See
  [workflows](automation.md).

`[agents].max_parallel` (1 to 16) is the number of sub-agent tasks that run at
once and, separately, the number of workflow steps that run at once. Tasks and
steps over the limit are queued.

## Scheduled runs and workflow size

A scheduled run is cancelled after 10 minutes (`RUN_TIMEOUT`,
`crates/otto/src/schedule/worker.rs`). The run is recorded with the failure
`the run did not finish within 10 min`. There is no configuration key for it.
A workflow definition must contain 1 to 32 steps (`MAX_STEPS`,
`crates/otto/src/workflow/service.rs`); a definition outside this range is
rejected when it is loaded.

## Stopping a turn that makes no progress

A turn whose model keeps issuing the same tool call, or keeps receiving tool
errors, would otherwise run until a deadline. Interactive turns have no deadline
unless you configure `turn_timeout`; chat and `--prompt` turns stop after
`unattended_turn_timeout` (default `30m`). Otto watches this itself and ends such
a turn with the error `no progress: the same <tool> call repeated <n> times in a row`,
`no progress: <n> tool calls in a row failed (last: <tool>)`, or
`no progress: a tool-call cycle repeated <n> times without progress (last: <tool>)`.
This applies to every turn, including each sub-agent's, which inherits your
limits. A turn that ends this way is not a persistence failure: the session
continues and you can send the next prompt.

Two calls count as the same call when the tool name and the arguments are the
same; key order and spacing in the arguments are ignored. Otto also detects
a cycle of 2 to 8 calls when the same calls return the same result text, and
the pattern repeats `no_progress_repeats` times. The results may be successes or
errors. For example, `read a`, `read b`, `read a`, `read b` with unchanged
contents, or an edit rejected before writing followed by a read of the same
unchanged file, cannot loop indefinitely. If any result in the cycle changes,
the match restarts, so polling whose output changes is not stopped.

Not detected:

- Cycles longer than 8 calls.
- A cycle that contains an operation that may have changed state (for example a
  `bash` call, a write or an edit that ran). Cycle tracking resets after such
  an operation; only declared read-only tools and operations known not to have
  started take part.
- Cycles when extension hooks are configured. Only the identical-consecutive-call
  and consecutive-failure checks run then, because hook effects are not covered
  by tool safety.

Use `agent_status` and `agent_cancel` for these cases; chat and `--prompt` turns
also have the unattended deadline.

`[agent].no_progress_repeats` (default 8) and
`[agent].no_progress_failures` (default 12) set the limits. The repetition
limit applies to both identical consecutive calls and unchanged cycles of
2-8 calls: a cycle of period P stops the turn after P x `no_progress_repeats`
calls.
Set one to `0` to disable that condition; set both to `0` to disable the guard.
Raise them if a deliberate poll loop, such as watching a long build, is being
stopped early. Values must be between 0 and 1000.

### Logs and metrics

Every turn that stops this way writes one `turn_stalled` record to the
diagnostic file `~/.otto/logs/diagnostic-<pid>-<random-id>.jsonl` (see
[Logs, metrics, and request correlation](servers-and-connectors.md#logs-metrics-and-request-correlation)).
The record has `session_id`, `task_id` (present only for a sub-agent or workflow
step turn), `kind` (`repeated_call`, `repeated_failure` or `repeated_cycle`),
`tool` (the tool of the last call) and `count`. It never contains tool
arguments or results. Find it with
`rg turn_stalled ~/.otto/logs/diagnostic-*.jsonl*`.

`otto serve` also counts these stops in `otto_turn_stops_total{reason}` on
`/metrics`, with `reason` set to the same three `kind` values. The count covers
main-session turns, sub-agent turns and workflow step turns run by that process.
The counter exists only in `otto serve`; the terminal UI, `otto --prompt` and the
schedule worker write the log record but expose no metric. If the diagnostic file
cannot be opened at startup, Otto prints a warning and neither the record nor the
counter is produced.

## Turn budget

`[agent.budget]` bounds how much model work one turn and the work it starts may
use. Both limits are on by default.

| Key | Counts | Scope | Unit | Default |
| --- | --- | --- | --- | --- |
| `max_tokens` | `input_tokens + output_tokens` of every model response, summed over the root turn and everything it starts (see below). Input tokens already include cached input. Responses to automatic compaction requests and to the requests of a split-turn summary count too. | shared by the root turn and everything it starts | tokens | `4200000` |
| `max_steps` | Ordinary model calls: one for each pass of the agent loop, including the call after each tool batch. Compaction requests are not steps. A retry after a context overflow is the same step. | one controller turn (see below); each started turn counts its own | calls | `54` |

Both defaults are twice the 99th percentile of 150 local turns (2,096,868
tokens and 27 steps). `0` disables the limit it is set on; `0` for both
disables the budget. Values must be between 0 and 1000000000000 for
`max_tokens` and between 0 and 1000000 for `max_steps`; a negative or larger
value fails at load with an error that names the key.

### What shares a budget

A **root turn** is a turn that starts from new input: a prompt typed in the
terminal or sent to `otto serve` or `otto acp`, an image prompt, a stored
schedule prompt, or a chat message. Each root turn creates one budget with its
own `budget_id`, and it replaces the budget of the session's previous root turn.

- **Tokens are shared.** The root turn and everything it starts charge one
  `max_tokens` total: the sub-agents it starts (including a resumed task), each
  step attempt of a workflow it starts, and the wake and follow-up turns that
  run afterwards. Tokens used by a sub-agent therefore reduce what the parent
  and the other sub-agents may still use.
- **Steps are per controller turn.** `max_steps` counts the model calls of one
  turn of the controller. When a Bash approval interrupts a turn, the turn
  continues after the decision with the token total and step count it had
  before the approval; an approval does not reset either. Each sub-agent task,
  each workflow step attempt, each wake turn and each follow-up turn counts its
  own steps from 0 against the same `max_steps`.
- A sub-agent or workflow run keeps the budget of the root turn that started it
  after that turn has ended. A sub-agent that is still running when the shared
  tokens run out stops at its next check.
- A workflow run that no turn started (one started through the `otto serve`
  workflow routes, a fork, or a retry with `otto workflow resume --retry`) gets
  one root budget of its own, shared by all steps of the run.
- Sub-agents cannot start sub-agents, so no budget is shared with a deeper level.
- An external ACP sub-agent runs in its own process with its own model
  provider. Otto does not receive its token usage, so it is not charged to the
  budget; only the turns Otto runs itself are.
- A manual `/compact` is not part of a turn; it runs with a new budget of its
  own. So does a run that has no controller turn (for example `otto eval`).

### When the check runs

- Otto adds the tokens of a response to the shared total as soon as the response
  arrives. The usage that a provider omits counts as 0.
- Before each ordinary model call, Otto stops the holder if its own calls
  already equal `max_steps` or the shared tokens used are at least `max_tokens`.
  With `max_steps = N`, a turn makes at most N calls.
- Before a compaction request and before the retry that follows an overflow
  compaction, Otto stops the holder if the shared tokens used are at least
  `max_tokens`, so a summary request cannot add usage after the limit was
  reached.
- Nothing is cancelled when the shared tokens run out. Each holder of the budget
  (the parent turn, each running sub-agent, each running workflow step) stops
  at its own next check. A call is never interrupted while it streams. A holder
  that was running when the limit was crossed can therefore make at most one
  more model call and run the tool batch of that call; with several holders
  running, the total can exceed `max_tokens` by that amount for each of them.
  The tool calls of the last response still run, and each has its result in the
  transcript.

### What you see

A turn that reaches a limit ends as failed with one of these errors; the text
names the key to change:

```text
budget exceeded: 4200311 of 4200000 tokens used in this turn ([agent.budget].max_tokens)
budget exceeded: 54 of 54 provider steps used in this turn ([agent.budget].max_steps)
```

The session stays usable. The next prompt starts a new root budget, and you can
ask the model to continue.

- **Sub-agent.** A sub-agent that stops at the budget ends with task status
  `failed`; its error text is the line above, and the parent receives the usual
  completion notification with that text.
- **Workflow step.** The step attempt ends as `failed` with the same text as its
  error, and the run fails. A budget stop is never retried: Otto does not retry
  failed workflow steps automatically. To run the step again, use
  `otto workflow resume RUN_ID --retry STEP_ID`; that retry starts a new root
  budget.
- **Wake and follow-up turns.** A wake turn delivers sub-agent, timer and
  workflow notifications. It is not started when the shared tokens of the
  session's most recent root turn are used up. The notifications stay queued and
  are delivered with your next input, which starts a new root turn with a new
  budget. Nothing is shown at that moment in the terminal UI, the line REPL,
  `otto serve` or `otto acp`; the log record below is the only trace.
  `otto --prompt` has no next input, so it ends with exit status 1 and prints
  `N queued notification(s) were not delivered because the token budget is used
  up ...` instead of exiting as if everything had been delivered. The step limit
  never withholds a wake turn, because each wake turn counts its own steps.
- **Follow-ups.** A follow-up that the model queued with `task_queue` lives only
  as long as the turn that queued it. When the shared tokens are used up, the
  follow-ups are not started and are discarded with a visible notice that names
  the key; queue the work again in the next turn.
- **First wake after a restart.** The budget is in memory only. If a session has
  had no root turn in the current process (for example a notification that is
  pending right after a restart), the first wake turn starts a root budget
  with the configured limits.

### Not persisted

A budget is never written to a session file or a database. After a restart,
recovered work gets a new budget with the same limits: a sub-agent task you
resume, a workflow run that continues, and the first turn of a loaded session.
The tokens a root turn used before the restart are not counted again.

### Logs and metrics

Each holder that stops writes one `turn_budget_exceeded` record to the
diagnostic file (see [Logs, metrics, and request correlation](servers-and-connectors.md#logs-metrics-and-request-correlation))
with `session_id`, `task_id` (present only for a sub-agent or workflow step
turn), `budget_id`, `limit` (`tokens` or `steps`), `limit_value` and `used`. One
exhausted root can write several records, one for each holder that stops; they
share `budget_id`. A run that has no controller turn writes an empty
`budget_id`. A record contains no message or tool content. Find them with
`rg turn_budget_exceeded ~/.otto/logs/diagnostic-*.jsonl*`.

A wake turn that is not started writes one `turn_budget_wake_skipped` record for
each root budget, with `session_id`, `budget_id` and `pending_notifications`
(the number of queued notifications, never their text). A second refusal for the
same root writes nothing; the next root budget can write one again.

`otto serve` counts the stops in `otto_turn_stops_total{reason}` with
`reason="budget_tokens"` or `reason="budget_steps"`, next to the no-progress
reasons. Each holder's stop is counted, so one exhausted root can add more than
1. The metric exists only in `otto serve`; the terminal UI, `otto --prompt` and
the schedule worker write the log record only.

### Raising or disabling a limit

Set the key in `config.toml` and restart long-lived processes:

```toml
[agent.budget]
max_tokens = 8000000   # raise
max_steps = 0          # no step limit
```

Raise a limit for work that legitimately needs many model calls, such as a large
refactoring, or for a turn that starts several sub-agents, which all charge the
same `max_tokens`. Disable both only for turns that are bounded another way, for
example by `unattended_turn_timeout` and by your own review of the output.

## Removed keys

These keys now fail to load with an unknown-field error that names the key, in
the user, project and local configuration files alike. Delete the lines.

| Key | Why it is gone |
| --- | --- |
| `[agent].max_turns` | It never limited anything: no code read it. Use the deadlines and no-progress limits above. |
| `[web]` table, including `[web].allow_hosts` | Public web access needs no host list. The network policy is `[sandbox].network`; see [web tools](safety-and-tools.md#web-tools). |

---

[Documentation index](../README.md) · [User manual](../user-manual.md)
