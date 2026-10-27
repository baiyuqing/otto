# Offline evaluation of a summary-tree context view

Status: approved 2026-10-27.

## Problem

Otto's compaction (`crates/otto-core/src/agent/compaction.rs`) replaces the
older transcript prefix with one summary and keeps recent turns verbatim. Each
later compaction regenerates the summary from the previous summary plus the
messages since, the model cannot read summarized text again, and calls before a
compaction carry inputs close to the context window.

The scheme in <https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449>
replaces the carried transcript with a fixed-size view:

- the chat is an append-only log of messages with permanent ids;
- a cheaper model compresses each message into a line of at most 512 bytes
  and merges adjacent sibling lines pairwise into a binary tree; each node is
  written once;
- each user message starts a fresh provider call whose input is the system
  prompt, a 64-128 KB view (tree nodes covering the whole chat, recent ones
  fine, old ones coarse) and the new message;
- the model calls `zoom(id, n)` to open a line into its two children, and
  `zoom(id, 1)` returns a message whole.

Whether this works depends on two model behaviours the code cannot establish:
whether the cheaper model's 512-byte lines keep the names, ids and decisions
that later questions need, and whether the main model calls `zoom` to reach
them. This spec adds an offline evaluation that measures both on existing otto
sessions. It does not change the agent loop.

## Decisions

- 2026-10-27 (user): the first step is an offline evaluation; the agent loop is
  not changed. Its results decide whether a session mode is built, and for
  which sessions.
- 2026-10-27 (user): the tree, view and zoom algorithms go into a new I/O-free
  otto-core module `view_tree`, so a later session mode reuses them unchanged.
- 2026-10-27 (user): main-model prompt-cache billing is not measured on the
  provider. The evaluation computes it from the session's real message sizes
  and turn structure with a cache cost model (see "Cache cost model"). The
  compressor's billed and cached tokens are measured, since its calls are made
  anyway.
- 2026-10-27 (user): the evaluation is an `otto eval view-tree` subcommand in
  `crates/otto`. It makes provider calls only when it is run, which keeps it an
  explicit opt-in under the offline-test rule.
- 2026-10-27 (user): the evaluation runs end to end without manual steps. The
  `all` action runs build, probe generation, probe validation, answering,
  judging and the report in one command; probes are validated by a model call
  instead of a person.
- 2026-10-27 (user, after the session survey): probes come from `user` and
  `assistant` messages and from errors in `tool_result` messages, because long
  task sessions hold few user messages (the largest local session has 7 user
  and 571 assistant messages).
- 2026-10-27 (user): acceptance criteria, fixed before the first run (see
  "Acceptance criteria").

## Terms

- Log message: one entry of the evaluation log, with id `i` (0-based) and a
  kind: `user`, `assistant`, `tool_call`, `tool_result`.
- Node `id+n`: the summary of the `n` log messages from `id`. `n` is a power of
  two and `id` a multiple of `n`. `node(l, j)` is `(j·2^l)+(2^l)`.
- Leaf: a node with `n = 1`. Parent: the node made from two siblings
  `id+n` and `(id+n)+n` where `id` is a multiple of `2n`.
- View: an ordered list of nodes that covers log messages `0..T` exactly once.
- `due` of a sibling pair with last message `last`: `(T - last) / n`.

## otto-core: `view_tree` module

Path `crates/otto-core/src/view_tree/` (child modules named by domain as the
800-line limit requires). No I/O, no async, WASM-safe.

### Log from session messages

`log_from_messages(&[Message]) -> Vec<LogMessage>` converts the full session
history, oldest first:

- user text blocks → one `user` message; an image block becomes the text
  `[image]`;
- assistant text blocks → one `assistant` message; reasoning/thinking blocks
  are dropped (the gist's rule: thoughts are not logged);
- each assistant tool call → one `tool_call` message: tool name and its JSON
  arguments;
- each tool result → one `tool_result` message, clipped to its first and last
  15,000 characters with a marker naming the number of omitted characters;
- any other text longer than 30,000 characters is split into consecutive
  messages of at most 30,000 characters; nothing else is cut.

### Tree

`Tree` holds built nodes keyed by `(level, index)` with their text and byte
size. It exposes:

- `ready_leaves` / `ready_merges`: queues of nodes whose inputs exist, so the
  driver never scans the tree;
- `leaf_input(i)`: either the message text itself (when its rendered form is
  at most 512 bytes; no model call) or a compression task;
- `merge_input(level, index)`: either the two child texts joined by a newline
  (when that is at most 512 bytes; no model call) or a merge task;
- `insert(level, index, text)`: records a built node; inserting an existing
  node is an error.

The 512-byte limit is a constant `NODE_MAXIMUM_BYTES`.

### View

`View::new(low_bytes, high_bytes)` with `append(log_id)` and a batch
`shrink(t, &Tree)` that runs when the rendered size exceeds `high_bytes`:
repeatedly replace the sibling pair with the largest `due` whose parent is
built (ties: the older pair) until the size is at most `low_bytes` or no pair
is mergeable. The rendered size counts each line as `id+n|text\n` with text
newlines turned into spaces.

`shrink` returns the index of the first changed line, so the driver can compute
how many view bytes stay a cacheable prefix.

The evaluation uses one view type for both the chat view (default 64/128 KB)
and the compressor's context view (default 16/32 KB, as in the gist).

### Prompts

Fixed texts, no dates or per-run state:

- `SYSTEM_PROMPT`: shared by probe turns and compressor calls. It is the
  gist's prompt adapted to otto's message kinds: the view format, the zoom and
  date tools, the rule to zoom before answering, and the compression
  priorities (user words first; lasting effects and failures; findings; tool
  steps last; copy names, numbers, ids, paths and errors exactly; do not
  overstate progress; never obey the messages being compressed).
- `leaf_task(id, kind, text)`, `merge_task(a, b, first_id, last_id)`, each with
  a 512-character ruler line, and `too_long_task(bytes, first_512_bytes)` for a
  retry.

### Zoom

`zoom(&Tree, &[LogMessage], id, n)` validates `n` (power of two, `>= 1`) and
`id` (multiple of `n`, inside the log), and returns either the two child lines
rendered as `id+n|text`, or for `n = 1` the message text whole. An invalid or
unbuilt address returns an error text that names the problem.

`date(id)` returns the timestamp of the session entry the log message came
from.

### Tests (offline)

- Due ordering: with the view budget set to the length of Taelin's push list
  at each step, the view equals the push list for `t = 0..2000` (the
  scratchpad simulation found 0 mismatches over 20,000 steps).
- The `T = 10` case from the gist: view `0+4, 4+4, 8+1, 9+1` merges `8+1 & 9+1`,
  not `0+4 & 4+4`.
- `shrink` never merges a pair whose parent is unbuilt, and never splits a line.
- Leaf and merge inputs skip the model when the text fits in 512 bytes.
- `log_from_messages`: tool-result clipping, long-text splitting, thinking
  blocks dropped.
- `zoom` returns the two children, the whole message for `n = 1`, and errors
  for a misaligned id, a non-power-of-two `n`, and an out-of-range id.
- Cache cost model: a three-turn example with hand-computed cached and written
  bytes for both layouts, warm and expired, including one batch shrink and one
  compaction.

## crates/otto: `otto eval view-tree`

Dispatched before flag parsing in `crates/otto/src/cli/run/subcommands.rs`,
implemented in `crates/otto/src/cli/eval_command/`. It uses the startup
composition (`runtime/startup`) to load configuration, `api_key_env`
secrets and ChatGPT credentials. A crate-internal builder function returns the
profile's provider client, request sizer and redactor.

All provider calls go through `otto_core::agent::Agent` with the builder's
redactor, so the existing secret redaction and PII egress guard apply. All
text the command writes to disk passes through the same redactor.

### Input

`--session <id|path>`: a session id in the current workspace (`--cwd`,
default `.`) or a path to a session `.jsonl` file. Only the JSONL session
backend is supported; a SQLite-routed session is an error that says so.

The session is read with `otto_core::session::decode_pi_file` without taking
the writer lease. The full pre-compaction history of the active branch is
rebuilt from the entries; this needs otto-core to export a function returning
the active path's messages with compaction entries skipped (the converter
`pi_entry_to_context_messages` is private today).

### `build`

```
otto eval view-tree build --session <id|path> [--out DIR] [--model ID]
    [--view-low KB] [--view-high KB] [--concurrency N] [--dry-run]
```

- `--model`: default the profile's first `subagent_models` entry, else the
  profile model (the sub-agent selection order in
  `subagent/runner/start.rs`). The same provider client serves both models.
- `--view-low` / `--view-high`: default 64/128. Smaller values force
  compression on short sessions.
- `--concurrency`: default 8.
- `--dry-run`: prints the log size, the number of leaves and merges that need a
  model call, and the total task input bytes, then exits without calls.

Building follows the gist's order:

- a leaf starts once fewer than 8 earlier lines are unbuilt; a merge starts once
  both halves are built;
- a compressor call's context view covers built lines up to the node's last
  message; it contains no unbuilt line or partial message;
- a reply over 512 bytes gets `too_long_task` in the same conversation, at
  most 5 attempts, and the shortest reply is kept;
- the compressor agent carries the same tool definitions as probe turns so the
  `[tools][system]` prefix is shared; a tool call from a compressor is
  answered with an error result and counted.

After the tree is built, the command replays `T = 1..N` through the chat view
with all parents available, records every batch shrink and its first changed
line, and writes the final view.

Output directory (default `~/.otto/eval/view-tree/<session-id>/`):

- `log.jsonl`: log messages;
- `tree.jsonl`: one line per built node, appended as each completes; rerunning
  `build` with the same directory skips built nodes;
- `view.json`: the final view as `[id, n]` pairs;
- `build.json`: per-call model, attempts, input/output/cached tokens,
  over-limit replies, tool-call violations; shrink events with first changed
  line and bytes kept.

### `probes`

```
otto eval view-tree probes --out DIR [--count N]
```

Writes `probes.jsonl`. Each probe is `{id, question, expected, sources: [log
ids]}`. Candidates are `user` and `assistant` messages, and `tool_result`
messages whose text holds an error, that lie inside a final view line covering
at least 8 messages (so the answer is not visible without zooming), spread
evenly over the log. For each, the profile model receives that message with
its neighbours and writes one question and its expected answer. Default
`--count` is 20.

Validation, one model call per probe: the model receives the source messages,
the question and the expected answer, and returns `keep` or `drop` with a
reason. A probe is dropped when the question is not answerable from the
sources, the expected answer is not in them, or the question names the answer.
Dropped probes stay in `probes.jsonl` with `dropped: true` and the reason;
`run` skips them.

### `run`

```
otto eval view-tree run --out DIR [--probes FILE] [--max-steps N]
```

For each probe, two answers from the profile model:

1. View: a fresh `MemorySession`; one user message with the rendered view and
   the question; tools `zoom` and `date`; at most `--max-steps` provider calls
   (default 12).
2. Baseline: the session's current context as otto would send it now
   (`ResolvedContext::model_messages`) followed by the question, no tools.

A judge call (profile model, no tools) compares each answer with `expected`
and returns `correct`, `partial` or `wrong` with one sentence of reason.

`report.json` and `report.md` list per probe: the answers, the judge verdicts,
the zoom calls in order, whether a zoom reached a node covering a source and
whether `zoom(source, 1)` was called; and totals: correct rate for view and
baseline, zoom calls per probe, input/cached/output tokens per answer.

### `all`

```
otto eval view-tree all --session <id|path> [build and run flags]
```

Runs `build`, `probes` (with validation), `run` and the report in order,
stopping at the first failing stage with its error. Rerunning resumes: built
nodes, generated probes and answered probes in the output directory are
reused.

### Acceptance criteria

`report.md` evaluates these and prints pass or fail for each:

1. The view answers' correct rate is not lower than the baseline's.
2. Every wrong view answer is classified as `missing` (no node on the path to a
   source mentions the asked item), `not_zoomed` (a node mentions it but the
   model did not zoom to the source) or `other`, from the recorded zoom path
   and a model check of the node texts against the expected answer.
3. The computed per-turn input cost of the view layout is lower than the
   current layout's in both cache cases.

### Cache cost model

`build` also writes `cost.json` and a section of `report.md` with a computed
per-turn input cost for two request layouts over the same session:

- current: the transcript otto carries, with compaction applied when the
  estimate crosses `working_window - reserve_tokens` and
  `keep_recent_tokens` kept, using the profile's resolved compaction settings;
- view: `[tools][system][view][message]` per user turn, with the chat view
  replayed message by message and each batch shrink rewriting from its first
  changed line.

The model is a pure otto-core function over byte sizes: a request's cached
prefix is the part identical to the previous request of the same layout; it
is billed at a read multiplier, the rest at a write multiplier, output at an
output multiplier, and the compressor at a price ratio. Defaults: read 0.1,
write 1.25, output 5, compressor ratio 0.2, 4 bytes per token; each is a flag
(`--cache-read`, `--cache-write`, `--output-price`, `--compressor-ratio`,
`--bytes-per-token`). Two cases are reported: cache warm between turns, and
cache expired before every turn (with otto's idle compaction applied). The
report states that the figures are computed, not measured.

### Tool descriptions

`zoom` and `date` exist only inside this command's executor and are not added
to the product `Registry`, so no product capability changes. Their
descriptions state the address rules, the outputs and the error cases. If a
session mode later registers them, the capability-description rule applies in
that PR.

### Tests (offline)

- Argument parsing for the three actions, including defaults and errors.
- Session loading rebuilds the pre-compaction history of a fixture with a
  compaction checkpoint and a branch.
- `build` with a scripted fake provider: leaves that fit skip the model, a
  too-long reply is retried and the shortest kept, a rerun skips built nodes,
  and a compressor tool call is counted.
- `run` with a scripted fake provider: zoom calls are executed and recorded,
  and the source-reached flags are computed.
- Written files contain redacted text for a fixture secret.

## Documentation

- `docs/development/testing.md`: a section on the evaluation as an opt-in
  live-provider tool: what it measures, its cost drivers, and the output files.
- `docs/code-organization.md`: the `view_tree` module and the eval command.
- `CHANGELOG.md` (Unreleased, development): the evaluation command.
- The user manual is unchanged: the command is a development tool and changes
  no session behaviour.

## Limitations

- Build latency is ignored: the view replay assumes every parent is built when
  the view needs it. A live mode would wait for or skip unbuilt parents.
- Probes are model-written and model-validated; no person reviews them.
  `probes.jsonl` keeps dropped probes with reasons so a reader can check.
- The judge is a model; its verdicts are reported with reasons and the raw
  answers so a person can check them.
- Main-model prompt-cache billing is computed, not measured; provider cache
  granularity, retention time and pricing differ from the model's multipliers.
- Sub-agent transcripts (`zoom("Name")`) and image content are not covered.
