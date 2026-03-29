# SOUL.md Design

## Purpose

`SOUL.md` is the durable identity contract for an agent.

It should answer:

- how the agent thinks
- how the agent collaborates
- what standards it holds
- how it sounds
- what it should never silently drift into

`SOUL.md` is not the same thing as:

- project instructions in `AGENT.md`
- long-term facts in `MEMORY.md`
- user-specific preferences in relationship memory

## Design Rules

### 1. Keep it stable

The soul should change slowly. If it changes every week, it is not identity.

### 2. Keep it behavioral

Describe tendencies and standards, not backstory fluff.

Good:

- prefers direct answers over motivational framing
- challenges weak assumptions when risk is high
- defaults to implementation over speculation

Bad:

- was born in the cloud
- loves solving problems with passion
- is a fearless genius hacker

### 3. Keep it runtime-neutral

Nothing in `SOUL.md` should mention `Codex`, `Claude Code`, or provider-specific prompt tricks.

### 4. Keep it reviewable

Soul changes should be explicit, diffable, and attributable.

## Recommended File Shape

```md
# Soul

## Identity
- This agent is a pragmatic development partner.
- It optimizes for correctness, momentum, and durable engineering quality.

## Temperament
- Direct under normal conditions.
- More conservative when data loss, production risk, or ambiguity is high.
- Prefers explicit assumptions over implicit optimism.

## Standards
- Favors small, reversible changes.
- Wants tests or verification for behavioral changes.
- Avoids hand-wavy claims about system behavior.

## Collaboration
- Acts autonomously when the path is clear.
- Stops to confirm when the cost of being wrong is high.
- Surfaces tradeoffs instead of hiding them.

## Voice
- Concise by default.
- Explains reasoning when the decision is non-obvious.
- Avoids theatrical or emotional framing.

## Boundaries
- Does not invent facts when evidence is missing.
- Does not silently rewrite identity based on one conversation.
- Does not let style override truth.

## Update Policy
- Relationship preferences belong in relationship memory, not here.
- Temporary task behavior belongs in session state, not here.
- Candidate soul changes require explicit approval.

## Revision Log
- 2026-03-28: Initial version.
```

## TypeScript Model

```ts
export interface SoulProfile {
  identity: string[];
  temperament: string[];
  standards: string[];
  collaboration: string[];
  voice: string[];
  boundaries: string[];
  updatePolicy: string[];
  revisionLog: SoulRevision[];
}

export interface SoulRevision {
  timestamp: string;
  summary: string;
  approvedBy: "human" | "policy";
}
```

## What Can Change Automatically

Usually safe:

- relationship memory
- episodic memory
- semantic memory with source attribution

Usually not safe:

- baseline tone
- risk posture
- standards
- autonomy policy

## Candidate Soul Delta Flow

The framework may detect possible identity changes, but should treat them as proposals.

```ts
export interface CandidateSoulDelta {
  summary: string;
  reason: string;
  evidence: string[];
  suggestedPatch: string;
}
```

Recommended flow:

1. detect repeated behavior or explicit user feedback
2. generate candidate delta
3. attach evidence from recent turns
4. require approval before applying
5. append a revision log entry

## Separation of Concerns

Use this split consistently:

- `SOUL.md`: stable identity
- `AGENT.md`: repo and project operating rules
- `MEMORY.md`: durable factual knowledge
- `relationship.md`: user preferences and collaboration patterns
- session summary: current thread continuity

If these layers blur together, prompt quality will decay over time.
