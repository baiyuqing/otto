# Agent Trace Toolkit

A lightweight workflow to correlate local file edits with LLM conversation context and visualize the links as SVG. The system watches your workspace, records per-file changes, extracts structural context with tree-sitter, and appends a structured entry to a Markdown log that can be rendered into a conversation↔change graph.

## What It Does

- Watches file modifications and captures diffs per save.
- Maps edited line ranges to AST nodes using tree-sitter (Python/TS/Go).
- Appends a structured entry to `docs/agent-trace.md`.
- Renders a two-column SVG that links conversation messages to changes.

## Requirements

- Node.js 18+
- Dependencies installed via `npm install`

## Setup

```bash
npm install
```

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

## Testing

An end-to-end test validates the minimal workflow: file change → log entry → SVG output.

```bash
npm test
```

## Log Format

Entries are appended to `docs/agent-trace.md`. Each entry includes a human-readable summary and a JSON block with:

- `timestamp` (UTC)
- `conversation` (id, message_id, role, created_at, excerpt)
- `file`
- `change` (added, deleted, hunks)
- `ast` (nodes intersecting modified lines)

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

- Excludes are defined in `scripts/trace_watch.py` (`DEFAULT_EXCLUDES`).
- Language mapping is defined in `scripts/trace_watch.py` (`LANG_BY_EXT`).
- SVG layout is implemented in `scripts/trace_svg.py`.

## Limitations

- This is a local workflow and does not capture un-saved buffer edits.
- The AST mapping is best-effort and depends on the tree-sitter grammar.
- SVG is a simple two-column graph; it does not yet support clustering or filtering.
