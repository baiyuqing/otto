# Executing a skill as a sub-agent

Status: approved 2026-09-22 (decisions settled by the user on 2026-09-22; see
"Decisions"). Not yet implemented beyond slice 1. This design is motivated by
external research, and Otto has no measurement of its own yet; the
"Validation" section below is the condition for adopting slice 3, not a
formality.

## Motivation

Piriyakulkij, Lawrence, Curth, Karmalkar and Prasad, *Subagents vs Agent
Skills: Executing Reusable Knowledge for Long-Horizon Agentic Tasks*
(arXiv:2609.09233v1) compares the two ways a skill package can be run:

- **Agent-skill execution** — the skill's `SKILL.md` body is loaded into the
  main context and the main policy follows it. This is what Otto does today.
- **Sub-agent execution** — a fresh context is seeded with the skill body and
  the delegated subtask; only the final answer returns to the main context.

The paper's result is *conditional*, and the condition is the whole point:

| Skill package | Which execution wins |
| --- | --- |
| Loosely structured knowledge, no stated inputs or outputs | **agent skill**, across every model tested |
| Procedural instructions with explicit input/output contracts | **sub-agent**, by a margin that grows as context pressure grows |

Two secondary findings matter for Otto:

- Sub-agent execution lowers *peak* context but raises *total* tokens
  substantially, because the delegating agent must restate what the child
  cannot see.
- Adding unrelated tool and skill descriptions to the initial context degrades
  accuracy under both modes; sub-agent execution degrades more gracefully.

This is external evidence obtained on a different harness (OpenHands) and a
different benchmark (SkillsBench). It is a reason to build the mechanism and
measure it, not a reason to change Otto's default.

## Why Otto is unusually well placed

Otto already has both halves and has never connected them.

`crates/otto/src/skill/mod.rs` gives `Skill { name, description, directory,
path }` plus a body loaded on demand. `crates/otto/src/subagent/definition.rs`
gives `Definition { name, description, body, directory, path, tools, model,
context, write_policy, write_paths }`. The first five fields are the same
data. The sub-agent runner already spawns a child with its own transcript, its
own tool subset, and a fixed delegation depth of one, and the `agent` tool
already takes `agent: <name>`, `prompt`, and `wait`.

So the paper's `E(Subagent(s), x) = r_T` is, in Otto's terms, already spelled
`agent(agent = <skill>, prompt = x, wait = true)`. What is missing is only
that a skill cannot become an agent definition.

## Design

### The contract is declared, and it decides the execution mode

The paper writes a skill description as `d = (q_in, h, q_out)`: a summary plus
natural-language input and output contracts. Their packages express this as
prose inside `description`.

Otto should make it **structured frontmatter** instead:

```markdown
---
name: vulnerability-record-normalization
description: Convert a scanner JSON report into normalized CSV-ready records.
input: |
  - the scanner JSON report path
  - the required severity filter and CSV columns
output: |
  - a JSON list of records with Package, Version, CVE_ID, Severity,
    CVSS_Score, Fixed_Version, Title and Url fields
---
```

Two reasons to deviate from the paper's prose convention:

1. **It makes the mode decision deterministic rather than a judgement call.**
   A skill that declares both `input` and `output` is sub-agent-eligible; one
   that does not is agent-skill only. That is the paper's Figure 2 turned into
   a rule the code can apply, and it needs no model inference at startup.
2. **It fails safe.** Every skill that exists today has no `input`/`output`,
   so every skill that exists today keeps exactly its current behavior — which
   is the mode the paper shows is *better* for contract-less packages. The
   feature cannot regress an existing skill.

The frontmatter parser already accepts arbitrary keys and block scalars
(`skill/frontmatter.rs`), so this needs validation, not new grammar. Both keys
are required together: declaring one alone is a validation warning and the
skill stays agent-skill only, because a half-contract is the case the paper
identifies as failing.

### What the model sees

The prompt listing gains the contract for eligible skills, because the paper's
delegation argument is that the main agent must be able to tell *what to send*:

```
<skill name="…" location="…" exec="subagent">
  <summary>…</summary>
  <input>…</input>
  <output>…</output>
</skill>
```

Contract-less skills keep today's single-line form. An eligible skill's entry
is longer, which is a real cost against the initial-context finding, so the
listing budget work below is a prerequisite, not an optional companion.

### How it runs

An eligible skill is registered as a synthetic agent definition:

- `body` ← the skill's `SKILL.md` body.
- `description` ← summary plus contracts.
- `context` ← `fresh`, always. `inherit` would reintroduce exactly the context
  the mechanism exists to avoid.
- `tools`, `model`, `write_policy`, `write_paths` ← the definition defaults.
  A skill does **not** get to widen the child's tool set; the existing rule
  that a child never exceeds the parent's tools is unchanged.
- The child's seed names the skill directory, so the body's references to its
  own `scripts/` and `references/` resolve. The paper's Figure 9 template does
  the same thing, and it is the difference between a skill that works in a
  child and one that silently cannot find its files.

The `skill` tool still loads an eligible skill's body on request. Refusing
would remove the escape hatch for the case the main agent genuinely needs the
instructions inline — for example to compose two skills, which is the ability
sub-agent execution gives up.

### What this does not do

No hierarchy. The paper's appendix finds that a hierarchical library with
routing nodes run inline and leaf skills run as sub-agents beats every flat
arrangement, but that presumes a library large enough to need routing. It is
noted as follow-up work, not designed here.

## Prerequisite: the initial-context budget

The paper's distracting-skill experiment is direct evidence that a long
listing costs accuracy by itself. Otto's current listing has a defect that
this design would make worse, and it should be fixed first:

`skill::prompt_section` drops entries once the rendered section would pass 8
KiB, choosing them by the tail of an alphabetical sort, and reports one stderr
warning. A dropped skill is invisible to the model *and unreachable* — the
`skill` and `agent` tools are both keyed by name, and the model cannot name
what it was never shown. Adding contracts to entries brings the cap closer.

The fix is a uniform degradation ladder rather than a drop: render every entry
at the most detailed level that fits — full, then summary without `location`,
then name plus contract, then name only — so a realistic catalog always stays
wholly visible. Dropping remains only as a final, still-warned resort.

`subagent::prompt_section` has the identical defect and should get the same
treatment.

## Validation

The paper's finding is not transferable by assertion; Otto's provider set,
prompts and tool surface all differ. Adoption is gated on Otto's own numbers.

Otto already writes everything needed, in two places that do not overlap.
`scripts/skill-exec-measure.mjs` reads both and prints one report per session:

1. **Peak context** per window, from `~/.otto/usage.db`. A sub-agent's
   transcript is a `MemorySession` and never reaches disk, so the only record
   of its context is the usage row tagged with its task id; an empty task id
   is the main context.
2. **Total tokens**, which the paper expects to rise substantially. If Otto
   cannot reproduce a peak-context reduction, the feature is not worth its
   token cost and should not ship.
3. **Whether the model honours `exec="agent"`**, from the parent transcript,
   which is the only place the choice is visible: an inline load is a `skill`
   call naming the skill, a delegation is an `agent` call naming it. This is
   the signal that settles decision 2, and no other source has it.

Running it needs provider credentials and real tasks, so it is not part of
any gate; the script's own tests are, and they are offline.

Until those numbers exist, sub-agent execution is opt-in per skill — which the
contract declaration already makes it — and never a default.

## Decisions

1. **Structured `input`/`output` frontmatter, not the paper's prose
   convention.** Two declared keys make the mode decision a field check
   rather than an inference over prose, and every existing skill declares
   neither, so all of them stay on the path the paper shows suits them. The
   cost is a documented deviation from the Agent Skills format, and it is
   one-way: a skill written elsewhere still works here, while these two keys
   are ignored by tools that do not know them.
2. **An eligible skill is marked as agent-callable in the listing, and the
   `skill` tool still loads it inline.** The listing entry says which way the
   skill is meant to be run, so sub-agent execution is the default the model
   is steered towards rather than a coin flip; loading it inline stays
   possible because composing two skills is the capability sub-agent
   execution sacrifices, and there must be a way back.

   This is the one place the design goes beyond the paper's evidence. The
   paper compares *fixed* modes: one arm runs everything as agent skills, the
   other everything as sub-agents. It never tested letting the model choose.
   Steering rather than forcing is therefore a judgement, and the measurement
   in "Validation" has to cover it: if the model ignores the marking and
   loads eligible skills inline anyway, the marking is not doing its job and
   the choice has to be taken away from it.
3. **The listing shows the contract for eligible skills**, because without it
   the main agent cannot know what to send, which the paper identifies as the
   failure mode that makes sub-agents useless. The listing budget was fixed
   first, in slice 1, for exactly this reason.

## Slices

1. ~~Listing degradation ladder for skills and agents.~~ Done in #178.
2. `input`/`output` parsing, validation, and eligibility, with no execution
   change. `/skills` shows which skills are sub-agent-eligible.
3. Registration of eligible skills as agent definitions, with the fresh-context
   and skill-directory seeding above.
4. Measurement against slice 3 using the `usage` store: peak context, total
   tokens, and how often the model honours the agent-callable marking. The
   last of these decides whether inline loading of an eligible skill stays
   available at all.

## Reference

Wasu Top Piriyakulkij, Rachel Lawrence, Alicia Curth, Sushrut Karmalkar,
Niranjani Prasad. *Subagents vs Agent Skills: Executing Reusable Knowledge for
Long-Horizon Agentic Tasks.* arXiv:2609.09233v1, 7 Sep 2026.
