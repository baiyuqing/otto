# Memory Architecture

## Goal

Memory is a core capability of the agent framework, not an implementation detail of prompt assembly.

The memory system should let an agent:

- keep task continuity over long work
- remember stable facts without replaying transcripts
- accumulate practical experience from doing work
- collaborate across multiple agents without losing boundaries
- remain inspectable and correctable by humans

## Non-Goals

- treating full chat history as memory
- making vector search the source of truth
- letting runtime session state become canonical memory
- silently mutating `SOUL.md`
- optimizing for benchmark recall before engineering usefulness

## Recommendation

Adopt a hybrid memory model with four distinct layers:

1. `soul`
2. `working memory`
3. `factual memory`
4. `experiential memory`

This follows the paper's functional split while staying practical for development agents.

## Why This Split

### `soul` is not memory

`SOUL.md` defines identity, standards, and collaboration style.

It should change slowly and only through an explicit approval path.

### `working memory` is the highest-leverage layer

Most failures in development agents are not caused by missing long-term memory. They come from losing the current objective, the current plan, the current blockers, or the current ownership state.

So the first design priority should be a strong task-scoped working memory system.

### `factual memory` and `experiential memory` should not be mixed

Facts answer "what is true".

Experience answers "what tends to work".

If they are stored together, the agent starts treating tactics like facts and facts like anecdotes.

## Unified Mental Model

This design uses three lenses:

- `forms`: how memory is physically represented
- `functions`: why the agent needs the memory
- `dynamics`: how the memory changes over time

### Forms

#### 1. Token-level memory

This is the canonical form in V1.

It includes:

- SQLite records
- Markdown notes
- task summaries
- structured memory entries

This form is explicit, reviewable, and easy to debug.

#### 2. Latent memory

This is runtime session continuity.

It includes:

- local runtime sessions
- remote daemon sessions
- hidden conversational state inside the underlying runtime

Latent memory is useful, but it is not trustworthy enough to be the source of truth.

#### 3. Parametric memory

This is future work.

It includes:

- learned retrieval policies
- distilled skills
- tuned adapters

Do not make this a V1 dependency.

### Functions

#### 1. Working memory

Working memory is the active task frame.

It should hold:

- current objective
- current plan
- open questions
- active artifacts and files
- blockers
- approval state
- latest concise summary
- current owner agent

Working memory is task-scoped and expires aggressively.

#### 2. Factual memory

Factual memory stores stable, attributable facts.

Examples:

- user preferences
- project facts
- repo conventions
- environment details
- approved design decisions
- confirmed team and agent relationships

Every factual item should carry:

- scope
- source
- confidence
- last verified time
- supersession link when replaced

#### 3. Experiential memory

Experiential memory stores what the system learned from doing work.

Examples:

- which debugging path resolved a class of issue
- which review pattern reliably catches regressions
- which runtime behaves better for a task type
- which workflow failed repeatedly
- which prompt assembly pattern helped

Experiential memory should favor compressed lessons over raw event streams.

## Scopes and Visibility

Memory must have explicit scope.

Recommended scopes:

- `session`
- `task`
- `agent-private`
- `user-private`
- `project-shared`
- `team-shared`
- `published`

Rules:

- `working memory` is usually `task` or `session` scoped
- `factual memory` can be private or shared
- `experiential memory` is often `agent-private` first, then promoted to shared when validated
- internal agent dialogue must not automatically become published memory

## Storage Strategy

Use a hybrid storage model.

### File-backed, human-curated

Keep these inspectable in files:

- `SOUL.md`
- `MEMORY.md` as an entry point and high-level index
- curated notes in `notes/`

Recommended file layout:

```text
SOUL.md
MEMORY.md
notes/
  facts/
    user-preferences.md
    project-state.md
    relationships.md
  experiences/
    work-log.md
    playbooks.md
    runtime-quirks.md
```

### SQLite-backed, operational

Use SQLite for:

- working memory state
- memory candidates
- memory entry indexes
- retrieval stats
- supersession links
- activity-to-memory evidence links

This gives us a database-first operational core without losing human reviewability.

## Write Path

Memory should not be appended blindly.

Use a candidate pipeline:

1. observe turn artifacts
2. generate memory candidates
3. classify as `working`, `factual`, or `experiential`
4. evolve against existing memory
5. commit or reject

### Candidate Sources

The write path should read from more than chat text.

Sources:

- user messages
- agent outputs
- activity events
- task status changes
- approvals
- generated artifacts
- review results

### Candidate Types

```ts
type MemoryCandidateKind = "working" | "fact" | "experience";
```

Examples:

- fact candidate: "user prefers concise answers"
- experience candidate: "codex plus targeted tests worked well for UI slice review"
- working candidate: "blocked on reviewer sign-off"

## Evolution Rules

Memory quality depends more on evolution than on initial extraction.

Recommended evolution operations:

- deduplicate similar facts
- supersede outdated facts
- merge repeated experiences into a stronger lesson
- decay stale low-value memory
- archive cold episodic records
- promote repeated experience into a reusable playbook

Important rule:

- do not overwrite a fact silently when the system is merely uncertain

Use `supersedes` rather than destructive update whenever possible.

## Retrieval Policy

Retrieval should be policy-driven, not one giant search.

Recommended order:

1. session and task working memory
2. pinned factual memory for the current scope
3. small experiential shortlist for the current task type
4. deeper search only if confidence remains low

### Retrieval Budgets

Set explicit budgets per category.

Example:

- working memory: highest priority, strict size cap
- factual memory: small, high-confidence set
- experiential memory: at most a few compact lessons

Do not let verbose experiential logs crowd out current task state.

## Prompt Injection Strategy

Memory should enter the prompt in separate sections.

Recommended prompt order:

1. soul
2. project instructions
3. working memory snapshot
4. pinned facts
5. selected experience
6. task request
7. runtime shim

This makes it easier to inspect why a behavior happened.

## Multi-Agent Memory

Multi-agent work needs shared and private memory boundaries.

Recommended rules:

- each agent keeps private experiential memory
- the task owns shared working memory
- shared factual memory is published through manager or policy gates
- reviewer conclusions can update shared factual memory
- builder exploration should not automatically become public memory

In practice:

- `manager` writes public summaries
- `builder` writes private experience and task working state
- `reviewer` writes review conclusions and risk signals

## Trustworthiness Requirements

Memory needs trust controls from day one.

Every committed factual or experiential item should support:

- source attribution
- scope visibility
- confidence
- timestamp
- last verification
- conflict marker

The UI should eventually let users inspect:

- why something was remembered
- what evidence supports it
- who can see it
- what superseded it

## Suggested TypeScript Boundaries

```ts
interface WorkingMemoryState {
  taskId: string;
  objective: string;
  plan: string[];
  openLoops: string[];
  blockers: string[];
  activeArtifacts: string[];
  ownerAgentId?: string;
  summary: string;
}

interface MemoryEntry {
  id: string;
  kind: "factual" | "experiential";
  scope: "agent-private" | "user-private" | "project-shared" | "team-shared" | "published";
  title: string;
  content: string;
  confidence: number;
  sourceRefs: string[];
  lastVerifiedAt?: string;
  supersedes?: string;
  tags: string[];
}

interface MemoryCandidate {
  id: string;
  kind: "working" | "fact" | "experience";
  scope: string;
  content: string;
  evidenceRefs: string[];
  proposedBy: "policy" | "agent";
}
```

## V1 Recommendation

V1 should optimize for clarity and usefulness, not maximal novelty.

Build:

- strong task working memory
- factual and experiential writeback pipeline
- SQLite operational store
- file-backed curated notes
- deterministic retrieval

Delay:

- embeddings as the primary recall mechanism
- automatic soul mutation
- parametric memory
- cross-agent unrestricted memory sharing

## Open Questions

- when should private experience be promoted to shared playbooks
- how much of `MEMORY.md` should be generated versus hand-edited
- whether factual memory should require stronger verification gates than experiential memory
- how to expose forgetting and supersession clearly in the UI
