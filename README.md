# Agent Trace Toolkit

A lightweight toolkit to trace AI coding sessions: it links LLM conversations to code edits, extracts AST context, and visualizes the relationship for review and auditing. Designed for AI coding/agent workflows (ChatGPT/Codex/Claude), Git-friendly, and runnable offline.

## What It Does

- Watches file modifications and captures diffs per save.
- Maps edited line ranges to AST nodes using tree-sitter (Python/TS/Go).
- Appends a structured entry to `docs/agent-trace.md`.
- Renders a two-column SVG that links conversation messages to changes.
- Generates an interactive HTML graph for filters/search/drill-down.

## Requirements

- Node.js 18+
- Dependencies installed via `npm install`

## Quick Start

```bash
npm install
npm run trace:demo-ui                 # generate demo assets (html/json/svg)
open docs/demo-clean/agent-trace.html # view interactive graph locally
```

## Manuals

- Install guide: `docs/INSTALL.md`
- Codex plugin guide: `docs/CODEX_PLUGIN_MANUAL.md`
- Why this project: `docs/WHY.md`

## Usage

### 1. Start the watcher

```bash
npm run trace:watch -- --conversation-log /path/to/conversation.jsonl
```

Notes:

- The conversation log is expected to be JSONL (one JSON object per line).
- If no log is provided, entries are still recorded but the conversation fields will be null.

### 2. Generate the SVG

```bash
npm run trace:svg -- --log docs/agent-trace.md --out docs/agent-trace.svg
```

### 3. Generate interactive HTML UI

```bash
npm run trace:ui -- --log docs/agent-trace.md --out docs/agent-trace.html
```

This writes:

- `docs/agent-trace.html` (interactive graph with filter/search/detail panel)
- `docs/agent-trace.json` (normalized graph data)

### 4. One-command demo output

```bash
npm run trace:demo-ui
```

This generates a complete demo set in `docs/demo-clean/` and `demo/workspace/`.

### 5. Bookstore demo (TypeScript, in-memory + MySQL-ready)

```bash
npm run demo:bookstore
```

Then hit the API:

- List books: `curl http://localhost:3000/books`
- Preorder: `curl -X POST http://localhost:3000/preorders -H 'Content-Type: application/json' -d '{\"bookId\":\"1\",\"email\":\"you@example.com\"}'`

Notes: persists in memory by default; optional MySQL schema lives in `demo/bookstore/schema.sql` (install `mysql2` and wire a pool if you want real storage).

### Optional: Log Codex CLI conversations

If you use the Codex CLI and want `conversation.jsonl` to be written automatically, run the wrapper:

```bash
npm run codex:log
```

This will launch `codex --no-alt-screen` and append JSONL records to `docs/conversation.jsonl` by default.
Set `CODEX_CONV_LOG=/path/to/conversation.jsonl` to override the output location.

## Testing

Test coverage is split into unit checks and an end-to-end flow.

- Unit tests (`tests/trace_svg.test.mjs`) validate Markdown parsing, SVG rendering, and escaping.
- Unit tests (`tests/trace_watch.test.mjs`) validate diff math, hashing, JSONL parsing, and AST fallback.
- End-to-end (`tests/e2e.mjs`) validates file change → log entry → SVG output.

Run all tests:

```bash
npm test
```

## Log Format

Entries are appended to `docs/agent-trace.md`. Each entry includes a human-readable summary and a JSON block with:

- `schema_version` (log shape)
- `timestamp` (UTC)
- `conversation` (id, message_id, role, created_at, excerpt, optional model/tokens/latency/cost)
- `conversation_window` (recent messages for context)
- `file`
- `change` (added, deleted, hunks)
- `ast` (nodes intersecting modified lines plus language/grammar/confidence)
- `git` (head, branch, dirty)

## Repository Layout

- `scripts/trace_watch.ts`: file watcher and logger
- `scripts/trace_svg.ts`: SVG renderer
- `docs/agent-trace.md`: trace log (auto-appended)

## Supported Languages

- Python (`.py` / `.pyi`)
- TypeScript (`.ts` / `.tsx`)
- JavaScript (`.js` / `.jsx`)
- Go (`.go`)

## Customization

- Excludes are defined in `scripts/trace_watch.ts` (`DEFAULT_EXCLUDES`).
- Language mapping is defined in `scripts/trace_watch.ts` (`LANG_BY_EXT`).
- SVG layout is implemented in `scripts/trace_svg.ts`.

## Limitations

- This is a local workflow and does not capture un-saved buffer edits.
- The AST mapping is best-effort and depends on the tree-sitter grammar.
- SVG is a simple two-column graph; it does not yet support clustering or filtering.
