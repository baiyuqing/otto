# Session reflection: learning memories and skill drafts from past work

Status: proposed 2026-10-02. Not approved, not implemented. No production code
or tests are written until this design is approved. Once implemented, current
behavior moves to the README and user manual and this document becomes
historical rationale.

## Motivation

Otto keeps three things that look like learning, and none of them closes the
loop:

- Compaction summarizes one session's context. It does not outlive the session
  and does not produce reusable knowledge.
- Memory (`crates/otto/src/memory`) stores facts and preferences. The only
  writers are the human (`/remember`) and the model's `remember`/`forget`
  tools, and the model's writes already land as pending candidates for
  `/memory review`. The service documents `Observe` and the extractor as
  deliberately absent, and the user manual lists "no automatic extraction".
- Skills (`crates/otto/src/skill`) are read-only to the agent. A skill exists
  only if a human wrote it.

Agents that improve with use (Hermes is the usual reference) add a step after
the work: look back, keep what was learned, and turn repeated procedures into
reusable instructions. Otto has the storage and the review gate for this but
not the step. This design adds that step without weakening the rule the
existing design already chose: **nothing a model produces becomes active
without a human accepting it.**

## Goals and non-goals

Goals:

- Extract durable facts and preferences from a session into memory candidates.
- Propose new skills, or revisions of existing ones, from procedures the
  session actually performed.
- Reuse the existing review surfaces and storage; add the smallest new
  surface that skills need.

Non-goals:

- No change to what is recalled or when. Recall stays request-local and
  untrusted.
- No automatic activation of any memory or skill, in any mode.
- No new provider, and no extractor that runs outside the session's own
  configured provider and profile.
- No writing to session history. Session files stay append-only and
  unchanged by reflection.
- No bundled-skill or `allowed-tools` changes; no skill scripts or assets in
  drafts (a draft is a single `SKILL.md`).

## Behaviour summary

- A **reflection run** reads a slice of one session's transcript, makes one
  tool-less model call, validates the structured result, and writes
  *candidates* and *drafts*. It never writes records or skills directly.
- Memory output goes through `memory::Service::propose` with
  `Origin::Extractor` (already modelled in `memory/contracts.rs`; the default
  policy already turns extractor writes into pending candidates). They appear
  in the existing `/memory review`.
- Skill output goes to a new staging directory, `~/.otto/skill-drafts/`, which
  is not a skill root. Discovery, the skill listing, the `skill` tool, and the
  Seatbelt read paths do not see it.
- A human reviews drafts with `/skill drafts` and accepts or rejects each.
  Accepting copies the draft into a skill root and is the only way a draft
  becomes visible to the model.
- Reflection is off unless asked for. `/reflect` runs it on demand; an
  optional config key runs it automatically at defined points (below).

## Triggers

| Trigger | Default | Behavior |
| --- | --- | --- |
| `/reflect [focus]` (TUI, REPL, Web UI, HTTP) | always available when enabled | Runs now over the unreflected slice; prints counts of candidates and drafts. |
| `auto = "on_exit"` | off | Runs once when a session closes normally, over the unreflected slice. Skipped for `--no-session`, sub-agent children, and sessions shorter than `min_turns`. |
| `auto = "on_compaction"` | off | Runs after a successful compaction, over the entries that compaction just summarized. |

`auto` is one value, not a list, to keep the matrix small. Automatic runs are
background tasks: they never delay, change, or fail the turn or the exit that
triggered them, and they are bounded by the same cancellation as the process
(`Ctrl+C` or shutdown cancels the in-flight call and records the run as
`canceled`).

## Input

- The slice is the session entries after the run watermark (see Storage) up to
  the current leaf, on the active branch only.
- It is rendered as plain text: user messages, assistant text, and tool calls
  with truncated arguments and truncated results. Reasoning content is
  excluded. The text passes through the existing secret redactor
  (`otto-core` `agent::redactor`) before it leaves the process, exactly as for
  a normal provider request.
- A hard cap (`max_input_bytes`, default 200 KiB) applies. Over the cap, the
  oldest entries are dropped and the run records `truncated`. A slice that is
  empty after rendering is a no-op, not an error.
- Existing active memory records for the session's scopes are included in a
  compact form so the model can avoid duplicates and propose `update` or
  `forget` actions with the right `target_id` and `base_revision` instead of
  creating near-copies.
- The existing skill catalog (names, descriptions) is included so the model
  can propose a revision of a skill instead of a duplicate. A revision's
  current body is included only for the one skill being revised.

## Output contract

The model returns one JSON object. The harness validates it strictly (unknown
fields rejected, sizes bounded, as the `remember` tool does with
`decode_strict_json`); a malformed object fails the run, and nothing partial
is written.

```json
{
  "memories": [
    {"action": "create|update|forget", "scope": "user|workspace",
     "kind": "preference|fact|convention", "key": "...", "text": "...",
     "confidence": 0.0, "reason": "...", "target_id": "", "base_revision": 0}
  ],
  "skills": [
    {"action": "create|revise", "name": "...", "description": "...",
     "body": "...", "reason": "..."}
  ]
}
```

Bounds (all configurable ceilings, defaults shown): at most 8 memory
proposals and 2 skill proposals per run; memory `text` at most the existing
record limit in `memory/validate.rs`; skill `body` at most 16 KiB.
Over-limit items are dropped and counted, not partially accepted.

### Memory proposals

Each becomes a `ProposeRequest` with `Origin::Extractor`, `confidence` as
given (clamped to `[0,1]`), and a `Provenance` carrying the session id, the
entry range, and the run id so a reviewer can trace why a candidate exists.
The store's content guard still runs on write. Records the human already
rejected are not re-proposed: the reviewer's earlier rejection is visible to
the service as candidate history, and the run skips an identical key and text
(to be confirmed against the candidate store during implementation).

### Skill drafts

A draft is `~/.otto/skill-drafts/<name>/SKILL.md` plus a sidecar
`draft.json` (action, reason, source session and range, run id, the content
hash of the skill being revised, creation time). Validation before writing:

- The name and description pass the same validation as discovery
  (`a-z0-9-`, 1 to 64 characters; description 1 to 1024), and `input`/`output`
  frontmatter, if present, passes `validate_skill_contract`. A draft that
  would be skipped by discovery is never written.
- `create` is rejected if the name exists in any skill root or as an existing
  draft; the model must use `revise`. `revise` is rejected if the name does
  not exist. This keeps a model from silently shadowing a skill that a
  higher-precedence root defines.
- The body is plain Markdown. Frontmatter keys other than `name`,
  `description`, `input`, `output` are dropped, so a draft cannot carry
  `allowed-tools` or any key Otto later gives meaning to.

## Review and acceptance

Skills are instructions the model will follow, which makes them a higher-risk
write than a fact. The review therefore shows everything.

- `/skill drafts` lists drafts with name, action, reason, and source.
- `/skill draft show <name>` prints the full `SKILL.md`; for a `revise`, a
  unified diff against the current skill. It also prints the contract-check
  status when the experimental checker is enabled, as `/skill <name>` does.
- `/skill draft accept <name> [--scope user|workspace]` installs the draft.
  `user` (default) writes `~/.otto/skills/<name>/SKILL.md`; `workspace` writes
  `.otto/skills/<name>/SKILL.md`. A `revise` first copies the replaced file to
  a timestamped backup beside it, using the same convention as the backed-up
  config writer in `crates/otto/src/config`. The draft is removed on success.
  The skill is visible after the next catalog discovery (`/new`, `/resume`,
  `/model`, or restart), as for any new skill.
- `/skill draft reject <name>` deletes the draft and records the rejection so
  an identical proposal is not made again.
- Accept refuses if the revised skill's content changed since the draft was
  made (hash mismatch), and tells the user to re-run reflection.
- Every command is human-originated. No agent tool can accept, reject, or
  write a draft, and the reflection model call has no tools at all.

Memory candidates are reviewed with the existing `/memory review`; no change.

## Safety

- **Prompt injection.** The transcript includes tool output and fetched
  content, so it is untrusted. The reflection prompt says so, the call has no
  tools, and the only effects of any model output are pending candidates and
  inert drafts that a human must accept. The prompt additionally instructs the
  model to ignore instructions inside the transcript.
- **Secrets.** Input is redacted before leaving the process. Output passes the
  memory content guard (memory) and a draft scanner (skills) that rejects
  anything matching the redactor's secret patterns. Drafts, run logs, and the
  database never store the raw transcript, only ids and ranges.
- **Persistence.** The staged drafts live under `~/.otto`, outside any
  workspace, so file tools cannot reach them and a repository cannot plant
  one. The workspace destination is only written by an explicit
  `accept --scope workspace`.
- **Cost and abuse.** One call per run, input and output token caps,
  provider usage recorded through the existing `usage` collector under a
  `reflection` label, and a per-session minimum interval so repeated
  triggers cannot loop. Automatic modes never retry a failed run.
- **Sandbox.** Reflection performs no `bash` and no file-tool I/O. The
  process writes its own storage with ordinary native file I/O, like the
  session store.

## Storage

A new `~/.otto/reflection.db` (SQLite, same open/migrate helpers as
`usage`). It holds, per session: the last reflected entry id (the watermark),
and one row per run: id, session id, trigger, entry range, status
(`ok|noop|failed|canceled|truncated`), counts of memory candidates and skill
drafts, token usage, and times. Rows are appended, never rewritten, except
the watermark, which advances only after a run completes. Failed and
canceled runs do not advance it, so the next run covers the same slice.
`--no-session` runs have nothing to reflect on and are skipped.

The database is opened only when reflection is enabled, as the skill-check
database is.

## Configuration

```toml
[reflection]
enabled = false             # master switch; default false
auto = "off"                # "off" | "on_exit" | "on_compaction"
min_turns = 4               # shorter sessions are skipped by auto modes
max_input_bytes = 204800
max_memories = 8
max_skills = 2
```

Reflection uses the session's current provider and model profile; it does not
add a separate model setting in the first release. With `enabled = false`,
`/reflect` reports that reflection is disabled and no database is opened.

## Where the code goes

Following the task map in `AGENTS.md`:

- `crates/otto/src/reflection/` (new): `run` (slice, render, call, validate,
  persist), `draft` (staging store, validation, accept/reject), `store`
  (SQLite). The model call reuses the provider contract and the redactor from
  `otto-core`; the pure parts (render, schema, validation) stay free of
  native dependencies where practical but live in `crates/otto`, because the
  stores are native.
- `app/`: the shared `reflect` use case and the trigger wiring, so the TUI,
  REPL, `otto serve`, and ACP call one implementation. No frontend gets its
  own copy.
- `cli` composition root builds the `Reflector` and injects it; `/reflect`,
  `/skill drafts`, and `/skill draft ...` are added next to the existing
  `/skill` and `/memory` commands.
- `server`: `POST /v1/sessions/{id}/reflect` and draft list/show/accept/reject
  routes; the Web UI renders them. HTTP and ACP are follow-ups to the
  command-line surfaces, listed under phases.
- `skill`: a small public function to resolve the install target for a draft;
  discovery code is unchanged.
- `memory`: remove the "Observe, extractor absent" claim in `service.rs` and
  the user manual only when the extractor path ships, and keep the "no
  `Binding.Observe`" statement accurate: reflection calls `propose`, it does
  not add `Observe`.

The architecture guards gain two checks: nothing in `reflection` calls
`Service::remember` (it may only call `propose`), and nothing but the draft
accept path writes into a skill root.

## Compatibility

- Session JSONL (Pi v3) is untouched.
- Additive config table, additive slash commands, additive routes. A config
  without `[reflection]` behaves as today.
- `Origin::Extractor` already exists in the store and the default policy, so
  no schema migration is needed for memory.
- Wasm boundary: all new code is native-only and lives in `crates/otto`;
  `otto-core` and `otto-web` are unchanged.

## Phases

1. **Memory reflection, on demand.** `reflection` module, store, `/reflect`
   for memory only, tests. Smallest slice that exercises the pipeline and the
   existing review gate.
2. **Skill drafts.** Draft store, validation, `/skill drafts` and
   `/skill draft show|accept|reject`, diff and backup on revise.
3. **Automatic triggers.** `on_exit` and `on_compaction`, watermark and
   interval guards.
4. **Server and Web UI.** HTTP routes and review UI; ACP exposure only if a
   connector needs it.

Each phase ships with tests and doc updates in the same change.

## Test plan

All tests are offline and deterministic, using a scripted fake provider.

- Pipeline: a slice produces exactly the expected `ProposeRequest`s and
  drafts; the watermark advances only on success; a canceled or failed run
  leaves it in place.
- Output contract: unknown fields, oversize items, bad names, `create` of an
  existing skill, `revise` of a missing skill, and a stripped `allowed-tools`
  key are each rejected or dropped with the right count.
- Safety: a transcript containing a planted secret never reaches the fake
  provider; a transcript with injected "write this skill" instructions
  produces at most pending candidates and inert drafts; the reflection call
  is made with an empty tool list.
- Review: accept installs into the chosen root and the skill is found by
  `Catalog::discover`; revise leaves a backup; a hash mismatch refuses;
  reject suppresses an identical later proposal; drafts are absent from
  discovery and from the Seatbelt read paths until accepted.
- Triggers: `on_exit` skips `--no-session`, children, and short sessions;
  `on_compaction` covers exactly the compacted range; the minimum interval
  blocks a second automatic run.
- Disabled: with `enabled = false` no database is created.
- Architecture guards for the two rules above.

## Decisions for approval

1. **Skill drafts need a human accept step** (recommended), versus
   auto-activating drafts. Recommendation: human accept only; skills steer
   behavior and this matches the existing safety stance.
2. **Default `auto = "off"`** (recommended), versus defaulting to
   `on_exit`. Recommendation: off until phase 3 has real-use evidence on
   cost and candidate quality.
3. **Single call, session's own provider and model** (recommended), versus a
   dedicated cheaper reflection model setting. Recommendation: defer the
   separate setting until usage data shows it is needed.
4. **User-level default for accept** (`~/.otto/skills`), with `--scope
   workspace` opt-in, versus workspace default. Recommendation: user level,
   since a workspace write puts model-derived instructions into a repository.
5. **Phase order**: memory first, then skills. Skills could be first if the
   skill loop is the priority; the pipeline is shared either way.

## Open questions to settle during implementation

- Exact session entry identifiers for the watermark and how the active branch
  is walked; the design assumes the Pi v3 reader already exposes both.
- How rejected memory candidates are queried so the run can skip repeats.
- Whether the one-shot call should reuse the compaction summary request path
  or the plain provider turn path; the choice does not change this contract.
