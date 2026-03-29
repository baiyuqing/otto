# Collaboration UI

## Goal

The framework needs a front end that can show:

- direct messages between a user and an agent
- public channel conversations
- task threads under a channel
- internal agent-to-agent dialogue for the same task
- agent activity events such as shell work, GitHub writes, task-state changes, and memory writeback

The key design rule is that these are not the same surface, even if they all end up rendered in one UI.

## Core Model

Use a database-backed collaboration model first. Slack can later project into the same model.

```text
Conversation
  -> root DM
  -> root channel
  -> channel thread
  -> internal agent dialogue

Task
  -> owns one primary user-visible conversation
  -> may link one or more internal agent dialogues

Message
  -> belongs to one conversation
  -> carries sender, kind, visibility, and task linkage

ActivityEvent
  -> belongs to a task and optionally a conversation
  -> records what the agent did, not only what it said
  -> can be public, private, or internal
```

### Why this model

- DM and channel are different collaboration modes
- thread is the real unit of work
- internal agent coordination should be visible in the UI but should not pollute the public conversation
- users need to see agent actions such as shell work, GitHub comments, and memory writes
- Slack later becomes just one provider, not the core data model

## UI Surfaces

### 1. Inbox

The left rail should show three sections:

- Direct Messages
- Channel Conversations
- Agent Dialogues

Each item should show:

- title
- latest preview
- task status
- unread count
- last update time

### 2. Conversation View

The main pane should show a lane-based timeline.

Recommended lanes:

- user
- public agent
- worker agents
- system

Important rule:

- a channel thread and an internal agent dialogue should be switchable side by side for the same task
- the user should be able to see what the workers said to each other without mixing that transcript into the public channel timeline

### 3. Task Sidebar

The right rail should show:

- task title
- current status
- runtime target
- current owner agent
- linked conversations
- approval state
- recent activity events

This is where the UI explains how one public thread maps to one task and multiple internal conversations.

## Visibility Rules

- `dm` conversations may read private memory
- `channel` conversations may read shared project memory only
- `agent` conversations are internal and may include orchestration details
- internal agent messages must never be auto-published into channel conversations

The UI should make that difference explicit with labels like:

- `Private DM`
- `Channel Thread`
- `Internal Agent Dialogue`

## Database-First API Contract

The front end does not need Slack first. It needs stable read models.

For the current prototype, SQLite is the preferred first backing store. A seeded local database is enough to drive the collaboration UI before any Slack integration exists.

Recommended read APIs:

```text
GET /api/conversations
GET /api/conversations/:id
GET /api/conversations/:id/messages
GET /api/conversations/:id/activities
GET /api/tasks
GET /api/tasks/:id
GET /api/tasks/:id/conversations
GET /api/tasks/:id/activities
```

V1 can stay read-only. Message sending and approval actions can come later.

### SQLite bootstrap

Use the built-in seed command to create a local demo database:

```bash
npm run collab:seed-demo -- .otto/collaboration.sqlite
npm run collab:serve -- .otto/collaboration.sqlite --port 4318
cd web && npm run dev
```

This writes one shared demo snapshot containing:

- one private DM
- two shared channel threads
- one internal agent dialogue
- linked tasks and unread counts

The web preview should try `/api/collaboration` first. If the local SQLite HTTP server is not running, it may fall back to the bundled demo snapshot.

### 3-agent company bootstrap

Use the company seed command if you want to inspect the framework as a tiny software company:

```bash
npm run company:seed-demo -- .otto/company.sqlite
npm run collab:serve -- .otto/company.sqlite --port 4318
cd web && npm run dev
```

This writes a snapshot with:

- one private DM between the user and the manager
- one public channel thread
- one internal agent room for manager, builder, and reviewer
- one task tying the public and internal conversations together
- activity events that show what each agent did

## Recommended V1 Scope

Ship the smallest UI that proves the model:

1. list conversations grouped by `dm`, `channel`, and `agent`
2. open one conversation and render the timeline
3. show linked conversations for the same task
4. show task status and owner agent
5. show recent agent activity for the selected task
6. make internal agent dialogue visible without mixing it into the public thread

## TypeScript Boundaries

The framework should expose three layers:

- domain types in `src/collaboration/types.ts`
- database access interface in `src/collaboration/store.ts`
- front-end read model builder in `src/collaboration/view-model.ts`

This keeps the UI contract explicit even before a real web app exists.
