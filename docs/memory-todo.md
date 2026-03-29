# Memory TODO

## Goal

Turn memory into a first-class subsystem for the agent framework.

## Phase 0: Contracts

- [ ] Link the main architecture doc to the dedicated memory design
- [ ] Freeze the vocabulary: `soul`, `working`, `factual`, `experiential`
- [ ] Remove old memory naming such as `semantic` and `relationship` from future-facing docs and interfaces

## Phase 1: Core Types

- [ ] Define `WorkingMemoryState`
- [ ] Define `MemoryEntry`
- [ ] Define `MemoryCandidate`
- [ ] Define `MemoryEvidenceRef`
- [ ] Define `MemoryScope`
- [ ] Define `MemoryConflict`

## Phase 2: Storage

- [ ] Add a SQLite-backed memory store for operational state
- [ ] Add tables for working memory, factual memory, experiential memory, candidates, and evidence links
- [ ] Define the file-backed curated memory layout under `SOUL.md`, `MEMORY.md`, and `notes/`
- [ ] Decide which fields are canonical in SQLite and which are projections into Markdown

## Phase 3: Formation Pipeline

- [ ] Create a candidate extractor from messages
- [ ] Create a candidate extractor from activity events
- [ ] Create a candidate extractor from task state changes
- [ ] Create a candidate extractor from artifacts and review outcomes
- [ ] Add classification rules for `working`, `fact`, and `experience`

## Phase 4: Evolution Pipeline

- [ ] Add duplicate detection for factual memory
- [ ] Add supersession for stale or replaced facts
- [ ] Add experience merge rules for repeated successful patterns
- [ ] Add archival rules for low-value old episodes
- [ ] Add promotion rules from repeated experience to playbook

## Phase 5: Retrieval

- [ ] Implement deterministic retrieval order
- [ ] Add retrieval budgets per memory class
- [ ] Add scope-aware filtering
- [ ] Add fallback deep retrieval only when cheaper recall is insufficient
- [ ] Record retrieval traces for debugging and UI inspection

## Phase 6: Prompt Assembly

- [ ] Inject working memory as its own prompt layer
- [ ] Inject pinned factual memory as its own prompt layer
- [ ] Inject selected experiential memory as its own prompt layer
- [ ] Keep `SOUL.md` fully separate from memory injection
- [ ] Add prompt diagnostics so the user can see which memory classes were used

## Phase 7: Multi-Agent

- [ ] Define private versus shared memory rules for `manager`, `builder`, and `reviewer`
- [ ] Add shared task working memory
- [ ] Add gated publication from internal dialogue into shared factual memory
- [ ] Prevent internal exploratory chatter from being auto-promoted into public memory

## Phase 8: UI and Inspection

- [ ] Show memory evidence in the collaboration UI
- [ ] Show why a fact or experience was retrieved
- [ ] Show superseded and conflicting memory entries
- [ ] Show visibility scope for each memory item
- [ ] Add manual approve, reject, and pin flows

## Phase 9: Trust and Quality

- [ ] Add confidence and verification timestamps to factual memory
- [ ] Add memory conflict handling
- [ ] Add redaction rules for private memory
- [ ] Add retention and forgetting policies
- [ ] Add regression tests for memory leakage across scopes

## Suggested Build Order

1. strong task working memory
2. SQLite operational schema
3. factual and experiential candidates
4. deterministic retrieval
5. prompt-layer integration
6. UI inspection
7. multi-agent promotion and trust controls

## Immediate Next Tasks

- [ ] Refactor future-facing docs from `semantic/relationship` to `factual/experiential`
- [ ] Add `src/memory/types.ts` contracts for the new model
- [ ] Add a first SQLite schema for memory entries and working memory
- [ ] Connect task activities to memory evidence refs
- [ ] Add one end-to-end test that proves a task writes and retrieves a factual memory item
