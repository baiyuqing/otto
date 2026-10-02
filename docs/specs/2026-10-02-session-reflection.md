# Session reflection: learning memories and skills from past work

Status: approved 2026-10-02. Phase 1 (memory reflection on demand) and
phase 2 (generated skills with the vetting pipeline) are implemented; phases 3
and 4 (automatic triggers, HTTP and the Web UI) are not. The five open
decisions were answered on 2026-10-02 and are recorded under "Decisions"
below. Current user behavior of the shipped parts is in the user manual; this
document remains the rationale and the plan for the rest.

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
reusable instructions. Otto has the storage for this but not the step. This
design adds that step with two different trust levels, chosen deliberately:

- **Memory** keeps the rule the existing design already chose: a model's
  write is a pending candidate until a human accepts it.
- **Skills** are written and activated automatically, because a skill loop
  that waits on a human rarely closes. The cost is that model-derived
  instructions persist across sessions without a prior review, so this design
  bounds that with ownership rules, caps, visible notices, and a one-command
  revert (see "Skills" and "Safety").

## Goals and non-goals

Goals:

- Extract durable facts and preferences from a session into memory candidates.
- Create new skills, and revise skills that reflection itself created, from
  procedures the session actually performed.
- Reuse the existing review surface for memory and the existing skill roots
  for skills; add the smallest new surface that skills need (list and
  revert).

Non-goals:

- No change to what is recalled or when. Recall stays request-local and
  untrusted.
- No automatic activation of memory: model-originated records stay pending
  candidates.
- No automatic edit of a skill a human wrote or has since edited.
- No new provider, and no extractor that runs outside the session's own
  configured provider and profile.
- No writing to session history. Session files stay append-only and
  unchanged by reflection.
- No bundled-skill or `allowed-tools` changes; no skill scripts or assets in
  a generated skill (it is a single `SKILL.md`).

## Behaviour summary

- A **reflection run** reads a slice of one session's transcript, makes one
  tool-less model call, validates the structured result, and writes memory
  *candidates* and generated *skills*. It never writes memory records
  directly.
- Memory output goes through `memory::Service::propose` with
  `Origin::Extractor` (already modelled in `memory/contracts.rs`; the default
  policy already turns extractor writes into pending candidates). They appear
  in the existing `/memory review`.
- Skill output is written to `~/.otto/skills/<name>/SKILL.md`, the existing
  user-level skill root. The skill is picked up by the next catalog discovery
  (`/new`, `/resume`, `/model`, or restart), as for any new skill. Every
  write is announced to the user and can be undone with `/skill revert`.
- Reflection is on by default and runs automatically after a successful
  compaction. `/reflect` runs it on demand, and `[reflection]` can turn it,
  or just its skill output, off (below).

## Triggers

| Trigger | Default | Behavior |
| --- | --- | --- |
| `/reflect [focus]` (TUI, REPL, Web UI, HTTP) | always available when enabled | Runs now over the unreflected slice; prints counts of candidates and skills. |
| `auto = "on_compaction"` | **default** | Runs after a successful compaction, over the entries that compaction just summarized. Skipped for `--no-session` and sub-agent children. |
| `auto = "on_exit"` | not default | Runs once when a session closes normally, over the unreflected slice. Skipped for `--no-session`, sub-agent children, and sessions shorter than `min_turns`. |
| `auto = "off"` | not default | No automatic runs; only `/reflect`. |

`auto` is one value, not a list, to keep the matrix small. Because
`on_compaction` is the default, a session that never reaches compaction never
reflects on its own, which bounds the default cost to long sessions. Automatic
runs are
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
  can revise a skill instead of creating a duplicate. A revision's current
  body is included only for the one skill being revised, and only skills
  reflection is allowed to revise (see "Skills") are offered for revision.

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
     "confidence": 0.0, "reason": "...", "target_id": "", "base_revision": 0,
     "evidence": [{"entry": "<entry id>", "quote": "<verbatim text>"}]}
  ],
  "skills": [
    {"action": "create|revise", "name": "...", "description": "...",
     "body": "...", "reason": "...",
     "evidence": [{"entry": "<entry id>", "quote": "<verbatim text>"}]}
  ]
}
```

Bounds (all configurable ceilings, defaults shown): at most 8 memory
proposals and 2 skill proposals per run; memory `text` at most the existing
record limit in `memory/validate.rs`; skill `body` at most 16 KiB.
Over-limit items are dropped and counted, not partially applied.

### Memory proposals

Each becomes a `ProposeRequest` with `Origin::Extractor`, `confidence` as
given (clamped to `[0,1]`), and a `Provenance` carrying the session id, the
entry range, and the run id so a reviewer can trace why a candidate exists.
The store's content guard still runs on write. Records the human already
rejected are not re-proposed: the reviewer's earlier rejection is visible to
the service as candidate history, and the run skips an identical key and text
(to be confirmed against the candidate store during implementation).

## Skills

### Validation before any write

A skill is written only after it passes the vetting pipeline in "Safety:
vetting pipeline" (taint check, evidence verification, rule scan, model
review). The checks below are the structural part of it.

- The name and description pass the same validation as discovery
  (`a-z0-9-`, 1 to 64 characters; description 1 to 1024). A skill that
  discovery would skip is never written.
- The body is plain Markdown. Frontmatter is regenerated by Otto from `name`
  and `description` only: `input`, `output`, `allowed-tools`, and every other
  key are dropped. In particular a generated skill never declares a contract,
  so it is never registered as a sub-agent definition; delegation stays an
  opt-in a human makes by editing the skill.
- The body passes the secret-pattern scanner and a size cap.
- `create` is rejected if the name exists in any skill root (user, workspace,
  or bundled), so reflection can never shadow or replace a skill another root
  defines. The model must use `revise` for an existing one.
- `revise` is allowed only for a skill reflection owns (next section).
- At most `max_skills` writes per run and `max_generated_skills` skills in
  total (default 30), so a poisoned transcript cannot flood the directory.

### Ownership

Reflection records, in `reflection.db`, the name and content hash of every
skill file it writes. A skill is **reflection-owned** only while its current
file hash equals the last hash reflection wrote. A skill a human authored, or
a generated skill a human has since edited, is human-owned: reflection never
revises or removes it, and a `revise` proposal for it is dropped and counted.
This is what keeps hand-written skills safe without a review step.

### Writes, history, and revert

- The write is atomic (temp file in the same directory, then rename) and
  creates `~/.otto/skills/<name>/SKILL.md`.
- A `revise` first copies the replaced file to
  `~/.otto/skill-history/<name>/<timestamp>/SKILL.md`. The history directory is
  outside every skill root, so old versions are never discovered or loaded.
- `/skill generated` lists reflection-owned skills with run id, source
  session, time, and the reason the model gave.
- `/skill revert <name>` restores the previous version, or removes the skill
  if the run created it, for a reflection-owned skill. It is human-originated;
  no agent tool can call it.
- The existing `/skill set <name> disabled` also works on generated skills.
- `/skill <name>` marks a generated skill as such and shows its provenance.
- When a run writes a skill the user sees one system line naming the skill and
  the revert command. Under `on_compaction` the session is live, so the line
  appears immediately; the skill becomes visible to the model at the next
  catalog discovery.
- The contract checker, when enabled, is unaffected: generated skills carry
  no contract, so nothing is checked or delegated.

Memory candidates are reviewed with the existing `/memory review`; no change.

## Safety

- **Prompt injection.** The transcript includes tool output and fetched
  content, so it is untrusted. The reflection prompt says so and instructs the
  model to ignore instructions inside the transcript, and the call has no
  tools. Memory output is still only pending candidates. Skill output is the
  residual risk of choosing automatic activation: injected text that reaches
  the model's output could become a persistent instruction that later
  sessions load. The bounds are the vetting pipeline below, the validation and
  ownership rules above, the caps, an announced write, and `/skill revert`.
  Taint and evidence checks cut the injection surface sharply, but none of the
  layers can prove a plausible-looking procedure is safe. Setting
  `skills = false` removes the risk.
- **Secrets.** Input is redacted before leaving the process. Output passes the
  memory content guard (memory) and a skill scanner that rejects
  anything matching the redactor's secret patterns. Skill history, run logs,
  and the database never store the raw transcript, only ids and ranges.
- **Persistence.** Generated skills are written only to the user-level root
  under `~/.otto`, outside any workspace, so a repository cannot cause one to
  be written into itself. Reflection never writes `.otto/skills`.
- **Cost and abuse.** One call per run, input and output token caps,
  provider usage recorded through the existing `usage` collector under a
  `reflection` label, and a per-session minimum interval so repeated
  triggers cannot loop. Automatic modes never retry a failed run.
- **Sandbox.** Reflection performs no `bash` and no file-tool I/O. The
  process writes its own storage with ordinary native file I/O, like the
  session store.

### Safety: vetting pipeline

No single check stops a determined injection, so skill writes pass four
independent layers, in this order, and any failure drops the skill (counted
by reason, never partially written). The reflection prompt is a fifth,
weaker layer and is **not** a security boundary: it tells the model the
transcript is data and to ignore instructions inside it, which lowers the
odds of a bad proposal but cannot be relied on.

1. **Source isolation (taint).** Each transcript entry is classified before
   the model sees it. An entry is *external* when it is an MCP tool result, an
   inbound message that did not come from the user (the `inbound` inbox and
   chat connectors), or a `bash` call whose command invokes a network tool
   (`curl`, `wget`, `ssh`, `nc`, `git clone|fetch|pull`, `gh`, and similar). A
   slice containing any external entry is *tainted*. For a tainted slice:
   skill output is not requested at all (the prompt omits the skills field and
   the run records `skipped_tainted`); memory proposals are still made, as
   pending candidates a human reviews, and external entries are excluded from
   the text the model sees. `skill_source = "untainted"` (default) enforces
   this; `"any"` turns it off. The classification is best-effort: it cannot
   see network access hidden inside a script or a file the workspace already
   holds, so it narrows the injection surface rather than closing it. Workspace
   file reads and ordinary `bash` output are treated as local.
2. **Evidence verification.** Every skill, and every memory proposal, must cite
   transcript entries with verbatim quotes. The harness checks, in code and
   not by asking the model:
   - each cited entry exists in the slice and is not external;
   - each quote is a substring of that entry's text after whitespace
     normalization;
   - a skill cites at least one **user** message (the user asked for or
     approved the work) and at least one successful assistant tool call or
     result entry showing the procedure was actually performed;
   - a memory of kind `preference` cites at least one user message.
   Proposals that fail are dropped. The verified citations are stored as
   provenance and shown by `/skill <name>` and `/memory review`.
3. **Rule scan (deterministic, offline).** The skill name, description, and
   body are rejected, with the matching rule id recorded, when they contain:
   - a secret pattern from the redactor;
   - instruction-override or concealment phrasing (English and Chinese), for
     example "ignore previous instructions" or "do not tell the user";
   - tampering with Otto's safety controls: `--sandbox off`, approval bypass,
     or edits to Otto's config, skills, or memory directories;
   - destructive or egress commands: `rm -rf` outside a clearly scoped path,
     piping a download into a shell, or sending files or environment values to
     a network address;
   - a URL or host that does not appear in a cited entry;
   - invisible or control Unicode (zero-width, bidirectional overrides, tag
     characters), or an encoded blob longer than a small threshold.
   The rule set is a data table next to its tests so a miss becomes a one-line
   fix with a regression case.
4. **Model review.** A second, tool-less call, `skill_review = true` by
   default, sees only the candidate skill text and a fixed rubric (does it
   instruct the agent to exfiltrate data, bypass the sandbox or approvals,
   hide its actions, act outside the task it was derived from, or take
   destructive actions). It does not see the transcript. It must answer with a
   fixed `allow` or `reject` token plus a reason; anything else, an error, or a
   timeout is `reject` (fail closed). The reviewer reads the candidate body,
   which is itself untrusted, so this layer is probabilistic and can be
   fooled; it is a defense in depth, not a proof.

After the pipeline the earlier bounds still apply: ownership by content hash,
no contract or `allowed-tools`, per-run and total caps, an announced write,
and `/skill revert`.

The types enforce the order: `skillwrite` accepts only a `Vetted` skill value,
which only the vetting module can construct, so a new code path cannot write a
skill without passing the pipeline. A test and an architecture guard check
this.

Cost: a run is one reflection call plus at most `max_skills` review calls.

## Storage

A new `~/.otto/reflection.db` (SQLite, same open/migrate helpers as
`usage`). It holds, per session: the last reflected entry id (the watermark),
and one row per run: id, session id, trigger, entry range, status
(`ok|noop|failed|canceled|truncated`), counts of memory candidates and skill
writes, token usage, and times. Rows are appended, never rewritten, except
the watermark, which advances only after a run completes. Failed and
canceled runs do not advance it, so the next run covers the same slice.
`--no-session` runs have nothing to reflect on and are skipped. The database
also holds the ownership table: skill name, last written content hash, run id.

The database is opened only when reflection is enabled.

## Configuration

```toml
[reflection]
enabled = true              # master switch; default true
auto = "on_compaction"      # "off" | "on_exit" | "on_compaction"; default on_compaction
memories = true             # propose memory candidates
skills = true               # write generated skills
skill_source = "untainted"  # "untainted" | "any": allow skill writes only when the slice has no external entries
skill_review = true         # second-model review of each candidate skill
min_turns = 4               # shorter sessions are skipped by on_exit
max_input_bytes = 204800
max_memories = 8
max_skills = 2              # per run
max_generated_skills = 30   # in total
```

Reflection uses the session's current provider and model profile; it does not
add a separate model setting in the first release. With `enabled = false`,
`/reflect` reports that reflection is disabled and no database is opened.
Reflection is on by default, so the user manual documents how to turn it off.

## Where the code goes

Following the task map in `AGENTS.md`:

- `crates/otto/src/reflection/` (new): `run` (slice, render, call, validate,
  persist), `guard` (taint, evidence, rule scan, review; builds `Vetted`),
  `skillwrite` (ownership, atomic write, history, revert; accepts only
  `Vetted`), `store` (SQLite). The model call reuses the provider contract and the redactor from
  `otto-core`; the pure parts (render, schema, validation) stay free of
  native dependencies where practical but live in `crates/otto`, because the
  stores are native.
- `app/`: the shared `reflect` use case and the trigger wiring, so the TUI,
  REPL, `otto serve`, and ACP call one implementation. No frontend gets its
  own copy.
- `cli` composition root builds the `Reflector` and injects it; `/reflect`,
  `/skill generated`, and `/skill revert` are added next to the existing
  `/skill` and `/memory` commands.
- `server`: `POST /v1/sessions/{id}/reflect` and generated-skill list/revert
  routes; the Web UI renders them. HTTP and ACP are follow-ups to the
  command-line surfaces, listed under phases.
- `skill`: discovery code is unchanged; the user-level root path comes from
  the existing `roots` function.
- `memory`: remove the "Observe, extractor absent" claim in `service.rs` and
  the user manual only when the extractor path ships, and keep the "no
  `Binding.Observe`" statement accurate: reflection calls `propose`, it does
  not add `Observe`.

The architecture guards gain three checks: nothing in `reflection` calls
`Service::remember` (it may only call `propose`), nothing in `reflection`
writes to a workspace `.otto/skills`, only `skillwrite` writes into a
skill root, and `skillwrite` accepts only a `Vetted` value.

## Compatibility

- Session JSONL (Pi v3) is untouched.
- Additive config table, additive slash commands, additive routes. Behavior
  change: a config without `[reflection]` now gets the defaults, so after the
  triggers phase ships, existing users make an extra model call after each
  compaction and may gain generated skills. The release notes and the user
  manual must say so and show `enabled = false`.
- `Origin::Extractor` already exists in the store and the default policy, so
  no schema migration is needed for memory.
- Wasm boundary: all new code is native-only and lives in `crates/otto`;
  `otto-core` and `otto-web` are unchanged.

## Phases

1. **Memory reflection, on demand.** `reflection` module, store, `/reflect`
   for memory only, tests. Smallest slice that exercises the pipeline and the
   existing review gate.
2. **Generated skills.** The vetting pipeline (taint, evidence, rule scan,
   model review), ownership, atomic write, history, `/skill generated` and
   `/skill revert`, announcement line. Evidence verification also lands in
   phase 1 for memory proposals, so the pipeline starts there.
3. **Automatic triggers.** `on_compaction` (the default) and `on_exit`,
   watermark and interval guards. The defaults take effect when this phase
   ships; before it, only `/reflect` exists.
4. **Server and Web UI.** HTTP routes and review UI; ACP exposure only if a
   connector needs it.

Each phase ships with tests and doc updates in the same change.

## Test plan

All tests are offline and deterministic, using a scripted fake provider.

- Pipeline: a slice produces exactly the expected `ProposeRequest`s and
  skill writes; the watermark advances only on success; a canceled or failed run
  leaves it in place.
- Output contract: unknown fields, oversize items, bad names, `create` of an
  existing skill in any root, `revise` of a missing skill, and a stripped
  `allowed-tools`, `input`, or `output` key are each rejected or dropped with
  the right count; a generated skill is never registered as a sub-agent.
- Safety: a transcript containing a planted secret never reaches the fake
  provider; a transcript with injected "write this skill" instructions
  produces at most pending memory candidates and a bounded, validated skill;
  the reflection call is made with an empty tool list.
- Vetting: a slice with an MCP result, a non-user inbound message, or a
  `curl` command is tainted, skill output is skipped, and external text never
  reaches the model; a quote that is not verbatim, cites a missing or external
  entry, or has no user-message citation is dropped; each rule in the scan
  table has a positive and a negative case, including the Chinese phrases and
  invisible Unicode; the reviewer sees no transcript text, and a malformed
  answer, an error, or a timeout rejects; `Vetted` cannot be constructed
  outside the guard module (a compile-fail or architecture test).
- Skills: a written skill is found by `Catalog::discover`; `revise` leaves a
  history copy and updates the ownership hash; a human edit makes the skill
  human-owned so a later `revise` is dropped; a human-authored skill is never
  revised; `/skill revert` restores the previous version or removes a created
  skill; history is absent from discovery; the total cap is enforced;
  `skills = false` writes nothing; reflection never writes a workspace root.
- Triggers: the default is `on_compaction`; `on_exit` skips `--no-session`,
  children, and short sessions;
  `on_compaction` covers exactly the compacted range; the minimum interval
  blocks a second automatic run.
- Disabled: with `enabled = false` no database is created. The default test
  suite sets reflection explicitly with a fake provider, so defaults never
  cause a network call.
- Architecture guards for the three rules above.

## Decisions

Answered 2026-10-02:

1. **Skills activate automatically; no human accept step.** This differs from
   the recommendation (human accept only). It is recorded here together with
   the mitigations it required: ownership by content hash, no revision of
   human-owned skills, no contract or `allowed-tools`, caps, an announcement
   line, history, and `/skill revert`. Memory still lands as pending
   candidates.
2. **Default `auto = "on_compaction"`.** This also means reflection is enabled
   by default, because a default trigger needs the feature on. The default
   differs from the recommendation (off); cost is bounded to sessions that
   reach compaction.
3. **One call on the session's own provider and model.** A separate cheaper
   reflection model setting is deferred until usage data shows it is needed.
4. **User-level destination** (`~/.otto/skills`). Reflection never writes
   workspace `.otto/skills`.
5. **Phase order:** memory first, then skills, then automatic triggers.
6. **Add layered malicious-content defenses** (answered 2026-10-02): source
   isolation (taint), code-verified evidence quotes, a deterministic rule scan,
   and a fail-closed model review. The reflection prompt is kept but documented
   as not a security boundary.

## Notes from implementing phase 1

Questions the design left open, and what the code showed:

- **Reading the slice.** The live session no longer holds entries a
  compaction summarized, so a run reads the session file read-only
  (`Store::read_entries`) and follows the active branch. Entry ids are the
  watermark and the evidence ids.
- **Rejected candidates.** The memory store clears a rejected candidate's
  content, so a run cannot recognize and skip a rejected proposal. Only
  proposals already *pending* are suppressed. A rejected proposal can be made
  again by a later run.
- **Provenance.** The store refuses a non-empty `observation_id` on a
  candidate. The run id travels as candidate metadata (`reflection_run`) and
  the cited entry ids as provenance `message_ids`.
- **The request path.** A new `Agent::complete_text` in `otto-core` makes one
  tool-less request with the agent's own provider, model, thinking setting and
  redactor, redacts the input, and appends nothing to the session. It does not
  reuse the compaction summary path.
- **Usage.** The usage table's `kind` column only allows `provider` and
  `compaction`, so reflection calls are recorded as `provider` with the task id
  `reflection:<run id>` rather than adding a `reflection` kind and a schema
  migration.
- **The redactor is exact-value only.** It has no secret *patterns*, so the
  "secret pattern" part of the phase 2 rule scan needs new code; it cannot
  reuse the redactor.
- **`/memory review` did not exist.** The REPL and TUI listed it in their help
  but did not implement it. Phase 1 implemented it (list pending candidates,
  `accept|reject <id>`) because reflection's output is otherwise unreviewable.
- **Evidence for memories** is implemented in phase 1 (verbatim quotes,
  non-external entries, a user citation for `preference`, `update` and
  `forget`), as the phase list says.

## Notes from implementing phase 2

- **Pipeline order.** Output contract, then evidence (a skill must cite a
  user entry and an entry showing the work ran), then structure and the rule
  scan (`guard`), then a *precheck* of the name and ownership, then the model
  review, then the write. The precheck runs before the review so a proposal
  that cannot be written (name taken, not owned, cap reached) does not cost a
  review call.
- **`Vetted` is enforced by the compiler and by a guard test.** Its fields are
  private to `guard.rs`, so no other module can build one;
  `tests/reflection_boundary.rs` also fails if any file other than
  `skillwrite.rs` mutates the filesystem.
- **Ownership is the content hash.** `reflection.db` (schema version 2, with a
  migration from version 1) records the hash of every content reflection wrote
  per skill. Reflection owns a skill only while the file hashes to the latest
  one; any human edit makes it human-owned.
- **History is keyed by hash.** `~/.otto/skill-history/<name>/<hash>.md`
  holds every version written; `/skill revert` restores the previous hash's
  file, or removes the skill if reflection created it.
- **The rule scan** is a regex table in `guard.rs` (secret patterns, override
  and conceal phrasing in English and Chinese, tampering with sandbox,
  approvals and `~/.otto` state, pipe-to-shell, destructive and egress
  commands, uncited URLs, invisible Unicode, encoded blobs) plus an exact-value
  check through the run's redactor. Phrasings that read as ordinary
  engineering advice are covered by negative test cases, so the table can be
  tightened without regressions.
- **`memories = false`** still runs the reflection call when skills are on;
  the prompt asks for no memories and any returned are dropped. With both off,
  `/reflect` makes no model call.
- **Usage.** The review call is recorded under the task id
  `reflection:<run id>:review`.
- **The TUI** runs `/skill generated` and `/skill revert <name>` through the
  shared `skill_report`, but its argument suggestions do not list them yet.
- **Seatbelt.** `~/.otto/skills` is added to the sandbox read paths at process
  start only if it exists; a skills directory created by the first generated
  skill is readable by sandboxed commands after a restart.
