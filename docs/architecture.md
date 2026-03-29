# Dev Agent Framework Architecture

## Goal

Build a TypeScript-first framework for development agents where:

- `Claude Code` and `Codex` are replaceable runtimes
- the framework owns memory, prompt assembly, and agent identity
- the same agent can keep a stable personality across runtimes
- memory is durable, reviewable, and not trapped inside provider-specific sessions

## Non-Goals

- Rebuilding shell execution, file patching, or tool runtimes from scratch
- Building a general-purpose multi-agent orchestration system in v1
- Treating raw transcript replay as memory
- Letting personality override correctness or safety

## Core Idea

Treat the runtime as an executor and the framework as the persistent mind.

- Runtime: can read, write, call tools, and stream tokens
- Framework: decides what to remember, what to inject, how to behave, and how to stay consistent

The key abstraction is an `AgentKernel` that sits above runtime adapters. A runtime target may be local, such as `codex`, or daemon-backed, such as `remote:codex`.

## Architecture

```mermaid
flowchart TD
    A["User Turn"] --> B["AgentKernel"]
    B --> C["PromptAssembler"]
    B --> D["MemoryEngine"]
    B --> E["SoulEngine"]
    B --> F["SessionManager"]
    C --> G["RuntimeRegistry"]
    D --> C
    E --> C
    F --> C
    G --> H["RuntimeInventory"]
    H --> I["Local Targets"]
    H --> J["Remote Targets"]
    G --> K["RuntimeAdapter"]
    K --> L["Local Codex / Claude Code"]
    K --> M["Remote Daemon Adapter"]
    K --> N["Runtime Events"]
    N --> B
    B --> O["Reflection + Writeback"]
    O --> D
    O --> E
```

## Module Layout

```text
src/
  collaboration/
    types.ts
    store.ts
    view-model.ts
  core/
    agent-kernel.ts
    types.ts
    errors.ts
  runtime/
    adapter.ts
    inventory.ts
    types.ts
    codex-runtime.ts
    claude-code-runtime.ts
    remote-daemon.ts
  prompt/
    assembler.ts
    layers.ts
    policies.ts
  memory/
    engine.ts
    retrieval.ts
    evolution.ts
    writeback.ts
    types.ts
    stores/
      working-store.ts
      factual-store.ts
      experiential-store.ts
  persona/
    soul.ts
    soul-engine.ts
    policy.ts
  session/
    session-manager.ts
    compaction.ts
    types.ts
  skills/
    registry.ts
    types.ts
  storage/
    file-store.ts
    sqlite-index.ts
docs/
  architecture.md
  collaboration-ui.md
  memory-architecture.md
  memory-todo.md
  soul.md
```

## Design Principles

### 1. Files are the source of truth

Agent identity should remain inspectable without proprietary infrastructure.

- `SOUL.md` stores stable personality and standards
- `AGENT.md` stores project-level operating instructions
- `MEMORY.md` stores durable long-term notes
- daily logs and session summaries remain plain text or JSONL

Indexes can be rebuilt. Core agent state should be human-readable.

### 2. Deterministic recall before deep retrieval

Memory retrieval should not start with a global embedding search.

Recall order:

1. session state
2. task-local working memory
3. pinned factual memory
4. scoped experiential memory
5. deep retrieval
6. archived session summaries

This keeps behavior stable and reduces retrieval noise.

### 3. Prompt layers must be explicit

Do not build one giant opaque system prompt.

Prompt composition should be traceable as:

1. base policy
2. soul layer
3. project layer
4. task layer
5. memory injection
6. reflection patch
7. runtime shim

### 4. Personality changes are controlled

The framework may infer candidate soul changes, but it should not silently rewrite identity.

- factual and experiential memory can update automatically
- soul changes should require approval or an explicit policy gate

### 5. Ease of use beats ceremony

The framework should work well before the user writes custom config.

- ship with built-in skills
- auto-detect common workspace signals
- bootstrap sane prompt layers from the repo itself
- allow power users to override, but do not require hand-written setup on day one

## Runtime Layer

The runtime layer is split into three pieces:

1. `RuntimeDescriptor`: what target the user wants, for example `codex` or `remote:codex`
2. `RuntimeInventoryProvider`: where available runtime targets come from
3. `RuntimeAdapter`: how a target is actually executed

This split matters because the remote daemon is not a model runtime itself. It is a transport and discovery layer that exposes multiple runtimes behind one daemon session.

```ts
export type RuntimeTarget = string;
export type RuntimeAdapterId = string;
export type RuntimeFamily = "codex" | "claude-code" | "claude" | "demo" | "gemini";

export interface RuntimeDescriptor {
  target: RuntimeTarget;
  adapterId: RuntimeAdapterId;
  family: RuntimeFamily;
  label: string;
  transport: "local" | "daemon";
  capabilities: RuntimeCapabilities;
}

export interface RuntimeInventoryProvider {
  list(): Promise<RuntimeDescriptor[]>;
}

export interface RuntimeAdapter {
  readonly adapterId: RuntimeAdapterId;
  readonly capabilities: RuntimeCapabilities;
  readonly descriptor?: RuntimeDescriptor;

  createSession(input: CreateSessionInput): Promise<RuntimeSessionRef>;
  resumeSession(input: ResumeSessionInput): Promise<RuntimeSessionRef>;
  executeTurn(input: ExecuteTurnInput): AsyncIterable<RuntimeEvent>;
  interrupt(session: RuntimeSessionRef): Promise<void>;
}
```

### Adapter Responsibilities

- accept a resolved `RuntimeDescriptor`
- translate framework prompts into runtime-specific inputs
- normalize streamed events
- capture runtime metadata such as cost, latency, tokens, and errors
- expose runtime quirks via capability flags instead of leaking provider-specific logic everywhere

### Inventory Responsibilities

- list local static runtimes such as `demo`, `codex`, and `claude-code`
- list daemon-backed runtimes such as `remote:claude`, `remote:codex`, and `remote:gemini`
- keep target naming stable even if transport changes underneath

### Remote Daemon Modeling

The framework should model the remote daemon as:

- inventory source
- daemon transport
- multi-runtime host

It should not model the remote daemon as:

- a replacement for the agent kernel
- a replacement for soul or memory
- a single monolithic runtime id

### Adapter Non-Responsibilities

- deciding what memory to recall
- mutating `SOUL.md`
- selecting project context
- deciding whether a reflection should be persisted

## Memory Model

The framework should treat memory as a first-class subsystem rather than a prompt appendix.

Use three functional memory classes:

1. `working`
2. `factual`
3. `experiential`

Keep `SOUL.md` separate from all three.

### Working Memory

Working memory is the active task frame.

It should hold:

- the current objective
- the current plan
- open loops
- active files and artifacts
- blockers and pending approvals
- the latest concise task summary

This is the highest-priority memory for turn quality.

### Factual Memory

Factual memory stores stable facts the agent may need again.

Examples:

- user collaboration preferences
- project conventions
- environment facts
- accepted architectural decisions
- confirmed team relationships

Factual memory is not a chat log. Every item should be attributable and reviewable.

### Experiential Memory

Experiential memory stores lessons from doing work.

Examples:

- effective debugging paths
- review patterns
- known runtime quirks
- common failure modes
- previously successful task strategies

Experiential memory should compress repeated episodes into reusable guidance instead of growing as raw logs forever.

### Memory Dynamics

The memory lifecycle should follow three stages:

1. formation
2. evolution
3. retrieval

Formation creates memory candidates from turns, activities, task transitions, and artifacts.

Evolution deduplicates, supersedes, merges, forgets, and promotes memory over time.

Retrieval selects the smallest high-value subset needed for the next turn.

### Detailed Memory Design

The dedicated design and rollout live in:

- `docs/memory-architecture.md`
- `docs/memory-todo.md`

## Agent Kernel

The `AgentKernel` owns one turn end to end.

```ts
export interface KernelTurnInput {
  agentId: string;
  runtimeTarget: RuntimeTarget;
  workspacePath: string;
  userMessage: UserMessage;
  sessionHint?: string;
}

export interface KernelTurnResult {
  session: RuntimeSessionRef;
  outputText: string;
  writeback: WritebackReport;
  diagnostics: KernelDiagnostics;
}

export interface AgentKernel {
  handleTurn(input: KernelTurnInput): Promise<KernelTurnResult>;
}
```

### Kernel Responsibilities

1. load agent state
2. restore or create session
3. retrieve relevant memory
4. assemble prompt layers
5. call the selected runtime
6. collect runtime events
7. summarize and compact the turn
8. write episodic and semantic memory
9. propose soul deltas when appropriate
10. auto-select built-in skills for the current workspace

## Built-in Skills

Built-in skills are small instruction bundles that the framework can auto-enable from workspace signals. This is the main path to an easier out-of-the-box experience.

Examples:

- TypeScript workspace skill
- CLI product skill
- verification discipline skill
- project conventions skill

The intent is similar to OpenClaw's skill concept, but lighter:

- built-in first
- no manual marketplace install for common cases
- auto-selection from repo structure
- skills become a first-class prompt layer instead of an afterthought

```ts
export interface SkillDefinition {
  id: string;
  title: string;
  description: string;
  instructions: string[];
}

export interface SkillResolver {
  resolve(workspacePath: string): Promise<SkillDefinition[]>;
}
```

### Auto-Configuration Direction

The first useful auto-configuration pass should inspect:

- `package.json`
- `tsconfig.json`
- `tests/`
- `src/cli.ts`
- `AGENT.md` or `AGENTS.md`
- `SOUL.md`

From that, the framework should enable a small set of built-in skills without asking the user to wire anything by hand.

## Collaboration Model

The framework should support three collaboration spaces:

- `dm`: private user-agent conversation
- `channel`: shared team conversation or thread
- `agent`: internal agent-to-agent dialogue

These should be first-class types, not transport-specific details. A channel thread and an internal worker dialogue may belong to the same task, but they should remain separate conversations with different visibility rules.

```ts
export type ConversationSpaceKind = "dm" | "channel" | "agent";
export type ConversationKind = "root" | "thread";
export type ConversationVisibility = "private" | "shared" | "internal";

export interface CollaborationConversation {
  id: string;
  ref: ConversationRef;
  visibility: ConversationVisibility;
  title: string;
  participantIds: string[];
  taskId?: string;
  createdAt: string;
  updatedAt: string;
}

export interface CollaborationTask {
  id: string;
  title: string;
  status: CollaborationTaskStatus;
  ownerAgentId: string;
  runtimeTarget: string;
  primaryConversationId: string;
  internalConversationIds: string[];
}
```

### Collaboration Rules

- `dm` can use private memory
- `channel` can use shared project memory only
- `agent` can carry internal orchestration messages
- internal agent dialogue must not be auto-published into the public channel thread

This lets the framework show agent-to-agent discussion in the UI without polluting the team-facing conversation.

## Activity Events

Messages are not enough. The framework should also capture agent actions as first-class activity events.

Examples:

- shell command started / finished
- task status updated
- GitHub comment added
- channel history read
- memory file changed
- agent lifecycle state changed

These events should be stored separately from user-visible messages, but linked to the same task and conversation model.

```ts
export interface CollaborationActivityEvent {
  id: string;
  taskId?: string;
  conversationId?: string;
  actor: MessageSenderRef;
  kind: CollaborationActivityKind;
  visibility: ConversationVisibility;
  status: "started" | "completed" | "failed" | "info";
  title: string;
  detail?: string;
  payload?: Record<string, boolean | number | string | null>;
  createdAt: string;
  endedAt?: string;
  parentEventId?: string;
}
```

This gives the UI a way to answer a different question:

- messages answer "what did the agent say?"
- activities answer "what did the agent do?"

## Database-First Collaboration

For v1, a database can replace Slack as the collaboration substrate.

Recommended role split:

- files hold soul and curated long-term memory
- database holds conversations, tasks, messages, approvals, and session state
- Slack later becomes a projection or ingestion adapter, not the source of truth

Recommended read interface:

```ts
export interface CollaborationStore {
  listConversations(query?: CollaborationConversationQuery): Promise<CollaborationConversation[]>;
  getConversation(conversationId: string): Promise<CollaborationConversation | null>;
  listMessages(query: CollaborationMessageQuery): Promise<CollaborationMessage[]>;
  listParticipants(participantIds: string[]): Promise<CollaborationParticipant[]>;
  listTasks(query?: CollaborationTaskQuery): Promise<CollaborationTask[]>;
  getTask(taskId: string): Promise<CollaborationTask | null>;
}
```

This is the right place to back the system with SQLite first and Postgres later.

## Frontend Read Model

The UI should not query raw runtime events directly. It should consume a stable read model built from collaboration types.

Required views:

- inbox grouped by `dm`, `channel`, and `agent`
- timeline view for one conversation
- task sidebar with owner, runtime target, and linked conversations
- linked view between a public thread and internal agent dialogue for the same task

The read-model builder lives in `src/collaboration/view-model.ts`, and the higher-level UI design is documented in [docs/collaboration-ui.md](./collaboration-ui.md).

## Prompt System

Prompt construction should be a typed pipeline, not string concatenation spread across the codebase.

```ts
export type PromptLayerKind =
  | "base"
  | "soul"
  | "project"
  | "task"
  | "memory"
  | "reflection"
  | "runtime";

export interface PromptLayer {
  kind: PromptLayerKind;
  priority: number;
  content: string;
  source: string;
}

export interface AssembledPrompt {
  system: string;
  user: string;
  layers: PromptLayer[];
}

export interface PromptAssembler {
  assemble(input: PromptAssemblyInput): Promise<AssembledPrompt>;
}
```

### Recommended Prompt Policies

- stable identity goes into `soul`
- repo-specific norms go into `project`
- request-specific instructions go into `task`
- recalled facts stay short and attributed
- reflection patches should expire quickly unless promoted into durable memory

### Runtime Shims

Do not fork the entire prompt per runtime. Add a thin runtime layer instead.

Examples:

- Codex may need stronger tool-use formatting hints
- Claude Code may need different session continuation instructions

That difference belongs in `runtime` prompt layers, not in the soul or project layers.

## Memory System

Memory should be split by purpose rather than by storage backend.

```ts
export interface WorkingMemoryEntry {
  turnId: string;
  content: string;
  expiresAt?: string;
}

export interface EpisodicMemory {
  id: string;
  timestamp: string;
  taskSummary: string;
  outcome: "success" | "partial" | "failed";
  lessons: string[];
  relatedFiles: string[];
}

export interface SemanticMemory {
  id: string;
  topic: string;
  fact: string;
  confidence: number;
  sources: string[];
}

export interface RelationshipMemory {
  id: string;
  userId: string;
  preference: string;
  evidence: string;
  confidence: number;
}
```

### Memory Tiers

#### Working Memory

- scratchpad for the current task
- short TTL
- never treated as durable truth

#### Episodic Memory

- records what happened in a task
- stores failure patterns, wins, tradeoffs, and decisions
- should be derived from summaries, not raw logs

#### Semantic Memory

- distilled durable facts
- repo knowledge, workflow knowledge, stable preferences
- should cite source artifacts

#### Relationship Memory

- how the user prefers to collaborate
- response density, risk tolerance, autonomy level, communication style

#### Persona Memory

- stable identity, standards, and tone
- stored separately from task memory
- backed by `SOUL.md`

## Memory Interfaces

```ts
export interface MemoryRetrievalQuery {
  agentId: string;
  task: string;
  workspacePath: string;
  limit: number;
}

export interface MemoryRecall {
  pinned: string[];
  episodic: EpisodicMemory[];
  semantic: SemanticMemory[];
  relationship: RelationshipMemory[];
}

export interface MemoryEngine {
  recall(query: MemoryRetrievalQuery): Promise<MemoryRecall>;
  writeTurn(result: TurnWritebackInput): Promise<WritebackReport>;
}
```

## Soul System

The soul is not a vibe prompt. It is a durable operating profile.

Recommended split:

- identity: who this agent is
- temperament: how it behaves under uncertainty
- standards: what quality bar it holds
- collaboration: how it works with the user
- voice: how it communicates
- boundaries: what it should not do

The detailed format lives in [docs/soul.md](./soul.md).

## Session System

The framework should own a logical session model even if each runtime has its own native session mechanism.

```ts
export interface LogicalSession {
  agentId: string;
  logicalSessionId: string;
  runtimeSession?: RuntimeSessionRef;
  startedAt: string;
  lastTurnAt: string;
  summary?: string;
}
```

### Session Responsibilities

- map logical sessions to runtime sessions
- summarize long-running conversations
- compact old context into stable memory artifacts
- preserve continuity when switching runtimes

### Runtime Switching Rule

If the user switches from `Codex` to `Claude Code`, the framework should preserve:

- soul
- project instructions
- pinned memory
- relationship memory
- logical session summary

The new runtime should not inherit opaque provider-specific state unless the adapter knows how to restore it safely.

## Storage Model

Recommended MVP storage:

```text
agents/
  <agent-id>/
    SOUL.md
    AGENT.md
    MEMORY.md
    relationship.md
    sessions/
      <logical-session-id>.json
    turns/
      <turn-id>.jsonl
    memory/
      daily/
        2026-03-28.md
      episodic/
        <episode-id>.md
      semantic/
        <fact-id>.md
    index.sqlite
```

### Why this layout

- human-readable core state
- cheap local development
- simple git-friendly inspection
- clean migration path to remote storage later

## Turn Lifecycle

```ts
export interface TurnPipeline {
  loadAgentState(): Promise<void>;
  loadSession(): Promise<LogicalSession>;
  recallMemory(): Promise<MemoryRecall>;
  assemblePrompt(): Promise<AssembledPrompt>;
  executeRuntime(): Promise<RuntimeTranscript>;
  summarizeTurn(): Promise<TurnSummary>;
  writeMemory(): Promise<WritebackReport>;
}
```

Recommended writeback stages:

1. persist raw runtime transcript
2. generate turn summary
3. extract episodic memory
4. distill semantic memory
5. update relationship memory
6. emit candidate soul delta

## Suggested TypeScript Conventions

- use ESM
- enable `strict` mode
- keep domain types framework-owned and runtime-neutral
- validate all adapter IO at boundaries
- keep strings out of core orchestration where enums or tagged unions work better

Suggested boundary validation strategy:

- use plain TypeScript interfaces in the core
- use runtime schemas only at adapter and persistence boundaries

## MVP Scope

### In

- one local user
- one repo at a time
- `Codex` and `Claude Code` runtime adapters
- file-backed source of truth
- SQLite index for local search
- typed prompt assembler
- soul file with controlled updates
- session summarization and compaction

### Out

- distributed orchestration
- multiple users with separate tenancy
- rich GUI
- autonomous background swarms
- complex approval workflows

## Recommended First Milestones

### Milestone 1

- define core types
- implement file-backed soul and memory stores
- implement prompt assembler

### Milestone 2

- implement `CodexRuntime`
- implement `ClaudeCodeRuntime`
- add logical session manager

### Milestone 3

- add turn summarization
- add episodic and semantic writeback
- add retrieval ranking policies

### Milestone 4

- add controlled soul revision flow
- add runtime switching
- add diagnostics and observability

## Summary

This framework should be opinionated about identity and memory, but unopinionated about the underlying executor. TypeScript is a good fit because the problem is mostly about typed boundaries, event normalization, staged pipelines, and durable domain models rather than raw inference tricks.
