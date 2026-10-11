# Default deadline for unattended turns

Status: approved for implementation, 2026-10-11. The user asked for a limit on
unattended runs ("无人值守的"), accepted ("OK") a time deadline in place of a
step ceiling with interactive turns unchanged, and approved ("OK") this design
with the 30-minute default and the per-step budget.

## Current behavior

- The agent loop has no step or model-request ceiling. `[agent].max_turns` is
  parsed and ignored (`crates/otto-core/src/config/mod.rs:189`).
- `[agent].turn_timeout` is optional and unlimited when omitted; `0` is a
  configuration error (`optional_positive_duration`,
  `crates/otto-core/src/config/resolve.rs:373`). It applies to every turn.
- `Runner` builds the turn deadline from its `turn_timeout` field once per call
  in `run_with_system_context`, `run_followup_with_system_context` and
  `run_with_image_system_context` (`crates/otto/src/runtime/builder.rs`).
  A logical turn is one prompt step, at most 16 task-queue follow-ups
  (`MAX_FOLLOWUPS`, `crates/otto/src/app/task_control.rs:22`) and one retry
  step per approved command. Each of those steps is a separate call with a new
  deadline. Time spent waiting for an approval decision falls between steps
  and is not counted.
- The no-progress guard stops a turn after 8 consecutive identical calls or 12
  consecutive failed calls. A cycle that alternates between different calls is
  not stopped.
- Scheduled prompt runs stop after 10 minutes (`RUN_TIMEOUT`,
  `crates/otto/src/schedule/worker.rs:26`). Chat turns, `otto --prompt`
  turns and their sub-agents have no time limit by default.
- A chat turn carries a chat origin: `otto acp` parses it from
  `_meta.otto.chat` (`parse_origin`, `crates/otto/src/acp/mod.rs:1745`) and
  `otto acp --attach` relays it to `otto serve`. `App::begin_user_turn` stores
  it in `runner.schedule_origin` for the duration of the turn.

## Change

### Configuration

Add `[agent].unattended_turn_timeout`, a duration string with the same syntax
as `turn_timeout`.

| Value | Effect |
| --- | --- |
| omitted | `30m` |
| `"0"` or `"0s"` | no unattended deadline; `turn_timeout` alone applies |
| positive duration | that duration |
| negative or unparsable | configuration error naming the field |

It is an agent limit, so user, project and local configuration files may set
it, as they may set `turn_timeout` today. Managed policy does not constrain it.

### Which turns are unattended

1. A turn whose `runner.schedule_origin` is a chat origin
   (`telegram` or `feishu`), in `otto acp` and in `otto serve`.
2. Every turn of an `otto --prompt` process (`Frontend::Once`), including the
   wake turns that run while it waits for background tasks.

Wake turns do not call `begin_user_turn`; they use the origin of the last user
turn, so a wake turn in a chat session is unattended.

Not unattended: TUI, REPL, `otto --attach`, the web UI (`web` origin), editor
ACP clients (no origin) and scheduled prompt runs, which keep their 10-minute
limit and have no origin.

### Deadline selection

`Runner` gains `unattended_turn_timeout: Option<Duration>` and a `headless`
flag set by `cli/run.rs` for `Frontend::Once`. The three call sites listed
above use:

- unattended turn: the earlier of `turn_timeout` and
  `unattended_turn_timeout`, either of which may be absent;
- other turns: `turn_timeout`, as today.

`compact` and `complete_text` keep using `turn_timeout` only.

The budget stays per step: a prompt step, each follow-up and each approved
retry gets its own deadline. Without human input, one chat message can run at
most 17 steps of 30 minutes each (8.5 hours). Sub-agents already take the
earlier of `subagent_timeout` and the parent's remaining time
(`Deadline::child`), so they are bounded by the same deadline without change.

### On expiry

Nothing new is added. The step ends exactly as it does when a configured
`turn_timeout` expires today: the agent returns
`ProviderError::DeadlineExceeded` (`provider call deadline exceeded`), the
session history up to that point is kept, and the next prompt continues the
session. `otto --prompt` exits with `1`.

## Not changed

- No step or model-request ceiling; `max_turns` stays ignored.
- Interactive turns have no default deadline.
- `turn_timeout`, `provider_timeout`, `subagent_timeout`,
  `workflow_step_timeout` keep their semantics and defaults.
- Scheduled runs keep `RUN_TIMEOUT`.
- The expiry message still names the provider call even when the turn deadline
  expired.

## Alternatives rejected

- **Step ceiling.** Does not bound time: one bash step may run for
  `shell_timeout` and one model request for an unlimited `provider_timeout`.
  Otto had a 50-turn default and removed it in `b701f22` (2026-08-28).
- **One deadline for the whole logical turn.** Bounds a chat message to 30
  minutes in total, but approval waits fall between steps, so the deadline
  would either count the user's decision time or need pausing. Kept per step
  to match `turn_timeout`.
- **Reuse `turn_timeout` with an origin-dependent default and `0` for
  unlimited.** Cannot express "no limit in the TUI, one hour in chat", which is
  the setting a resident host needs.

## Compatibility

Chat and `--prompt` turns that ran longer than 30 minutes per step now stop.
Users who need the old behavior set `unattended_turn_timeout = "0"`. Existing
configuration files stay valid. Session history format is unchanged.

## Tests

- `config/resolve.rs`: omitted gives 30 minutes; `"0"` gives `None`; `"45m"`
  gives 45 minutes; `"-1s"` and `"x"` fail with an error naming
  `unattended_turn_timeout`.
- Deadline selection as a pure function over (`turn_timeout`,
  `unattended_turn_timeout`, unattended): each combination of absent/present
  values, and the earlier of two present values.
- `Runner` with paused time and a provider that never answers: a turn with a
  chat origin ends with `DeadlineExceeded` after 30 minutes; the same turn
  with a terminal origin is still running after 30 minutes; a headless runner
  applies the deadline to a terminal-origin turn.

## Documentation

- `docs/manual/getting-started.md`: config example and the `[agent]`
  deadline paragraph. Correct "follow-up work never reset that budget": each
  follow-up and approved retry starts a new budget.
- `docs/manual/interactive-use.md`: headless mode section.
- `docs/manual/safety-and-tools.md`: the no-progress section, which says
  deadlines are unlimited unless configured.
- `CHANGELOG.md`.
