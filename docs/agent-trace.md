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

### 2026-02-05T13:42:39Z
Conversation: id=null message_id=null role=null
File: `tests/e2e.mjs`
Summary: +151 -1; no AST matches
```json
{
  "trace_entry": true,
  "timestamp": "2026-02-05T13:42:39Z",
  "conversation": {
    "id": null,
    "message_id": null,
    "role": null,
    "created_at": null,
    "excerpt": null
  },
  "file": "tests/e2e.mjs",
  "change": {
    "added": 151,
    "deleted": 1,
    "hunks": [
      {
        "old_start": 1,
        "old_lines": 1,
        "new_start": 1,
        "new_lines": 151
      }
    ]
  },
  "ast": [],
  "summary": "+151 -1; no AST matches"
}
```