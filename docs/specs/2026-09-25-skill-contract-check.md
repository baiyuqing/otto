# Checking a skill's contract automatically (experimental)

Status: approved 2026-09-25. Implemented. Experimental: the rules,
the questions, the thresholds, the database schema and the `/skills` display
may change or be removed without a compatibility path. It is enabled at run
time through the `[experimental]` config table; it is built and tested like
every other feature, with no build-time switch.

## Motivation

[Skill sub-agent execution](2026-09-22-skill-subagent-execution.md) makes a
skill sub-agent-eligible when its frontmatter declares both `input` and
`output`. `validate_skill_contract` (`crates/otto/src/skill/mod.rs`) checks
only that both halves are present, non-blank and at most 1024 characters. It
cannot tell whether the declared contract is usable: whether the body is a
procedure, whether a child that starts with a fresh context can do the work
from `input` alone, and whether `output` names something the parent can use.
The research that design cites finds sub-agent execution better only for
procedural skills with explicit contracts, so a skill that declares a contract
it does not meet is delegated in the case where delegation performs worse.

This feature checks a skill's contract the first time the skill is delegated
to as a sub-agent, records the result, and shows it in `/skills`. No user
action starts a check.

## Behaviour summary

- Trigger: the `agent` tool starts a sub-agent from a skill-derived
  definition, and the database holds no record for that skill's current
  content and question set.
- The check runs in a background task. It never delays, changes or fails the
  delegation that triggered it.
- Deterministic rules run first. If a rule finds a defect, that is the result
  and TypeSafe is not called. Otherwise TypeSafe answers four questions.
- Every result is appended to `~/.otto/skill-checks.db`. Nothing is
  overwritten or removed.
- `/skills` shows the latest result for each skill that has one.
- The result is advisory. It never changes eligibility, never edits
  `SKILL.md`, and never reaches the model.

## Enabling

Disabled by default. Enabled only when all of these hold at runner build
time:

1. `[skills].enabled` is not `false` (existing key, default true).
2. `[experimental].typesafe_skill_check = true` in
   `~/.config/otto/config.toml` (new key, default false). Otto has no
   workspace config file, so a repository cannot enable the check.
3. `TYPESAFE_API_KEY` is set to a non-empty value.

```toml
[experimental]
typesafe_skill_check = true
```

When any condition fails no check is started, the database is not opened,
and `/skills` shows no results. Condition 3 gates the rules as well as the
request, so the feature is either fully on or fully off.

`[experimental]` is a new top-level table for features that may change or be
removed without a compatibility path. Unlike every other table it does not
deny unknown keys: an unknown key produces one warning and is ignored, so
removing an experimental feature does not turn a config that enabled it into
a parse error.

## Trigger and concurrency

- The hook is in `AgentTool::execute` (`crates/otto/src/subagent/tools.rs`),
  after the sub-agent has been started. It applies only to definitions that
  `extend_from_skills` registered from a skill; ordinary agent definitions and
  inline loads through the `skill` tool are not checked, because the
  questions are about sub-agent execution.
- The key is `sha256(SKILL.md bytes)` and `sha256(question set)`. A check
  starts only when the database has no row with that key.
- An in-process set of keys being checked prevents two concurrent
  delegations to the same skill from sending two requests. A second otto
  process can still check the same key at the same time; both rows are kept,
  which is harmless.
- The background task is not cancelled with the turn. It ends when its
  request ends, when the 30 s request timeout expires, or when the process
  exits; a check interrupted by exit writes nothing and is retried on the next
  delegation.

## Stage 1: deterministic rules

Rules can only establish a defect; no rule can establish that a contract is
good. A skill that no rule flags goes to stage 2.

| rule id | question it answers "no" to | fires when |
| --- | --- | --- |
| `context_reference` | `self_contained` | the body, case-insensitively, contains any of: `earlier in the conversation`, `previous message`, `as discussed`, `the conversation above`, `the user's last message` |
| `vague_output` | `output_usable` | `output`, trimmed and lowercased without trailing punctuation, has fewer than 4 words, or is one of: `the result`, `a result`, `a summary`, `the summary`, `the output`, `the answer` |

If any rule fires, the row records `source = rules` and the fired rule ids,
and TypeSafe is not called. The author has to edit `SKILL.md` anyway, and the
edit changes the key, so the next delegation checks the new content in full.

## Stage 2: TypeSafe

One request:

```http
POST https://api.typesafe.ai/v1/systemone
Authorization: Bearer $TYPESAFE_API_KEY
```

`model` is `"jev-latest"`. `state` is the full `SKILL.md` text, frontmatter
included. `questions` holds four entries:

| id | type | instructions | criteria |
| --- | --- | --- | --- |
| `procedural` | choice | Is the body of this skill a procedure to execute, or reference knowledge to consult? | `procedural`: ordered steps that take the declared input to the declared output. `knowledge`: facts, conventions or guidance without a fixed sequence. `mixed`: both, with neither dominant. |
| `self_contained` | noul | A sub-agent starts with an empty context and receives only what the `input` field describes. Can it carry out the body's steps without information from the conversation that delegated to it? | true: everything the steps need is named in `input` or in the skill package. false: the steps depend on earlier conversation, the user's recent messages, or files not named in `input`. |
| `output_usable` | noul | Does the `output` field describe a result the delegating agent can use directly, with its content and shape stated? | true: names the content and its form (fields, format, file path). false: vague, such as "a summary" or "the result". |
| `body_matches_contract` | score | Do the body's steps read the declared `input` and produce the declared `output`? | `["does not match", "partly matches", "matches"]` |

The question set, the rule list and their patterns are constants in the
source, and all of them feed the question-set hash, so editing any of them
re-checks every skill on its next delegation.

Errors:

- 429 and 529 are retried with exponential backoff (1 s, 2 s, 4 s), at most 3
  retries.
- 401, 422, other statuses, transport errors, a timeout, and a response
  missing any of the four answers or holding one of the wrong type end the
  check without writing a row, so the next delegation retries. Nothing is
  printed, because stderr output would overwrite the TUI screen (otto has
  no logging crate); instead the process keeps the last failure per skill
  in memory, and `/skills` shows it as `not checked yet (last attempt:
  <reason>)`, where the reason is the HTTP status or `timeout`,
  `transport error` or `invalid response`.
- The key is never written to the database, logs or output.

## Verdicts

Computed when displayed, from the stored answers and these thresholds; they
are not stored, so changing a threshold changes the display of existing rows
without new requests.

| id | hint when |
| --- | --- |
| `procedural` | choice is not `procedural` |
| `self_contained` | `noul` < 0.5 |
| `output_usable` | `noul` < 0.5 |
| `body_matches_contract` | `score` < 1.5, i.e. the nearest level is not `matches` (level 2) |

TypeSafe returns a Score answer as a probability-weighted number between
the level indexes (0 = `does not match`, 1 = `partly matches`, 2 =
`matches`), not as a level name. A Choice or Score answer with `confidence`
< 0.6 is shown as uncertain. Noul
answers carry no confidence in the TypeSafe API and are shown by value alone.
0.5 and 0.6 are initial values, to be adjusted against skills whose
suitability is known. A `rules` row shows each fired rule as a hint on its
question and the other questions as not asked.

## Storage

`~/.otto/skill-checks.db`, SQLite, one table, append-only:

| column | content |
| --- | --- |
| `id` | integer primary key; the latest row for a key has the largest id |
| `content_sha256` | hash of `SKILL.md` bytes |
| `questions_sha256` | hash of the question set and rules |
| `skill` | skill name at check time |
| `path` | absolute `SKILL.md` path at check time |
| `source` | `rules` or `typesafe` |
| `model` | model version from the response (`jev-1.13.0`); empty for `rules` |
| `checked_at` | RFC 3339 UTC |
| `result` | JSON: fired rule ids, or the four answers as returned |

SQLite rather than a JSON file because several otto processes (TUI sessions,
`otto serve`) can check skills at the same time; SQLite serialises their
appends. The schema has a `user_version`; a database with an unknown version
is left untouched, and the feature prints one warning to stderr at startup,
before any UI starts, and stays off for that process. Rows are never pruned.

Only `SKILL.md` is hashed: a change to the package's `scripts/` or
`references/` does not trigger a re-check. A newer model behind `jev-latest`
does not trigger one either.

## Display

`/skills` gains one line per sub-agent-eligible skill that has a row for its
current key: the source, the model, the check time, and one verdict per
question. A skill whose current content has no row shows `not checked yet`.
Rows for earlier content are not shown.

## Code placement

- `crates/otto-core/src/config/mod.rs`: `File` gains
  `experimental: Experimental` with `typesafe_skill_check: Option<bool>`,
  without `deny_unknown_fields`, skipped when serializing its default so the
  config writer does not add an empty table. Plain data; the wasm boundary is
  unaffected.
- `crates/otto/src/skill/check.rs`: rules, question set, hashing, TypeSafe
  client (reqwest, base URL and key passed in), verdicts, and the SQLite
  store. Native only.
- `crates/otto/src/subagent/tools.rs`: the trigger in `AgentTool::execute`,
  through a checker handle the runner is built with; `None` when the feature
  is disabled.
- `crates/otto/src/cli`: the composition root builds the checker when the
  three enabling conditions hold, and `/skills` reads the latest rows.
- AGENTS.md task map: add "automatic contract check" to the `skill` entry.
- User manual: one section marked Experimental, stating the enabling
  conditions, that `SKILL.md` content is sent to TypeSafe on first
  delegation, the database file, and that everything may change or be
  removed.

## Tests

All offline; TypeSafe is a local HTTP stub.

- Enabling: each of the three conditions, when unmet, leaves the runner
  without a checker, and a delegation sends nothing and opens no database.
- Config: `[experimental]` absent resolves to disabled; an unknown key under
  it parses, warns and is ignored; writing a config that never had it does
  not add the table.
- Trigger: a skill-derived delegation starts one check; an ordinary agent
  definition and a `skill` tool load start none; a key already in the
  database starts none; two concurrent delegations send one request.
- Rules: each rule fires on its pattern and not on a near miss; a fired rule
  writes a `rules` row and sends no request.
- TypeSafe: the request carries the four questions and the bearer header; 429
  then 200 writes one row after a retry; 401, a timeout, and a response
  missing an answer write nothing.
- Storage: rows append; editing `SKILL.md` adds a row under a new key and
  keeps the old one; an unknown `user_version` disables the feature.
- Verdicts: each threshold on both sides, and the low-confidence case.
- `/skills`: checked, not-checked-yet, and rules-sourced skills display as
  specified.
- The delegation that triggers a check returns the same result, in the same
  time, whether the stub answers, fails or hangs.

## Privacy

`SKILL.md` text is sent to `api.typesafe.ai` the first time each version of
a skill is delegated to, and only when all three enabling conditions hold.
Skills flagged by a rule are not sent. The user manual states this.
