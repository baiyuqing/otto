---
name: agent-trace
description: Track local file edits, map changes to AST nodes with tree-sitter, link them to the latest LLM conversation message, and render a conversation-to-change SVG. Use when users ask to set up, run, or extend the agent trace workflow, troubleshoot the watcher, adjust log parsing, or generate the SVG visualization.
---

# Agent Trace

## Overview

Use this skill to wire a local "conversation <> code change" trace system. The workflow watches file saves, records per-file diffs, maps them to AST nodes via tree-sitter, appends structured entries to a Markdown log, and renders a two-column SVG linking conversations to changes.

## Workflow

1. Confirm the conversation log location and format (JSONL).
2. Start the watcher to append entries to `docs/agent-trace.md`.
3. Generate the SVG from the Markdown log.
4. If mappings look wrong, adjust the log parser or AST language mapping.

## Commands

- Start watcher:
  - `npm run trace:watch -- --conversation-log /path/to/conversation.jsonl`
- Generate SVG:
  - `npm run trace:svg -- --log docs/agent-trace.md --out docs/agent-trace.svg`

## Conversation Log Mapping

The watcher expects JSONL and will use the last valid line as the "latest" message. It reads fields in this order:

- `conversation_id` or `id`
- `message_id` or `id`
- `role`
- `created_at` or `timestamp`
- `content` (string or list)

If the local log uses different field names, update `getLatestConversation()` in `scripts/trace_watch.ts` and note the change in the log header.

## AST Mapping

Language support is determined by file extension in `LANG_BY_EXT` inside `scripts/trace_watch.ts`. Update this mapping when adding new languages or custom extensions.

## Output

Entries are appended to `docs/agent-trace.md`. Each entry includes a summary line and an embedded JSON block with:

- `timestamp` (UTC)
- `conversation` (id, message_id, role, created_at, excerpt)
- `file`
- `change` (added, deleted, hunks)
- `ast` (nodes intersecting modified lines)

The SVG renderer (`scripts/trace_svg.ts`) reads the JSON blocks and draws a two-column graph with linking lines.

## Troubleshooting

- If no entries are recorded, install dependencies: `npm install`.
- If AST nodes are empty, confirm the language grammar exists and the extension is mapped.
- If conversation fields are null, confirm the JSONL log path and schema.
