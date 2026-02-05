# Agent Trace Log

This file captures per-file modification events and links them to the most recent
LLM conversation message available from the local conversation log. Each entry is
human-readable and includes a machine-readable JSON block.

## Schema

Each entry appends a JSON block with `"trace_entry": true`. The JSON schema is
stable and is what the SVG generator reads.

Fields:
- `timestamp`: ISO-8601 time in UTC.
- `conversation`: `id`, `message_id`, `role`, `created_at`, `excerpt`.
- `file`: Repo-relative path.
- `change`: `added`, `deleted`, `hunks`.
- `ast`: list of nodes that intersect modified lines (`type`, `name`, `start`, `end`).

## Entries

Entries are appended below by `scripts/trace_watch.py`.
