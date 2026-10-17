# File-backed memory store

Status: approved 2026-10-17.

## Problem

Memory records and candidates live in a Turso database
(`[memory.turso].path`, default `~/.otto/memory/memory.db`, or the endpoint
of a published `memory` storage route). Ordinary tools
cannot read it: `sqlite3` stops at the Turso FTS schema entry
(`__turso_internal_fts_dir_memory_records_search_key ... USING`), and
`otto memory status` opens the storage route authority, so it exits with
`storage is frozen for migration or its route changed` when that authority is
frozen.

The FTS index adds little at the current data size. One local installation
holds 41 active records on 2026-10-16, at most 23 in one workspace scope
(about 5.9 KB of text and keys). The index uses Turso's `default` tokenizer
(tantivy `SimpleTokenizer`, lower-casing, 40-byte token limit). It splits on
`!char::is_alphanumeric()`, so a run of CJK characters between punctuation is
one token, and a run longer than 13 characters exceeds 40 bytes and is not
indexed. A Chinese message matches a record only through ASCII words or an
identical short CJK run.

## Decisions

- 2026-10-17 (user): files are the source of truth. The memory database is
  removed.
- 2026-10-17 (user): no full-text index.

Proposed in this document, pending approval:

1. Recall stops matching the message text. It injects every in-scope record,
   newest first, until `max_results` and `recall_tokens`.
2. A legacy SQLite `memory.db` (before #295, 2026-10-03) is not importable by
   this release; it must first be migrated with `otto storage migrate` from
   Otto 0.4.8 or earlier.
3. `memory` leaves the storage framework: no route, no `storage transfer`,
   no `storage migrate`.
4. Two-file operations are not atomic across files; the crash windows listed
   under [Limits](#limits) are accepted.

## Layout

The store root is `<home>/.otto/memory/` (mode 0700). It is not configurable
and does not follow storage routes.

```text
identity.json          {"database_id":"<id>","user_scope_id":"<id>"}
records/<id>.md        one active record
tombstones/<id>.json   one forgotten record, without content
candidates/<id>.json   one pending or decided candidate
.lock                  writer lock
```

Files are mode 0600. An `<id>` is 1 to 64 characters from `[0-9A-Za-z_-]`;
Otto mints 32 lowercase hex characters as today.

## Record file format

```markdown
---
scope: {"namespace":"workspace","id":"sha256:60ef1..."}
kind: "preference"
key: "reply-language"
labels: []
confidence: 1.0
revision: 3
created_at: "2026-10-03T10:08:00.123456789Z"
updated_at: "2026-10-16T10:25:41.052684000Z"
expires_at: null
source: {"origin":"human"}
metadata: {}
---
Reply to technical questions in Chinese.
```

- Every frontmatter line is `name: <JSON value>`. All eleven names are
  required, each once; an unknown name is an error. JSON is also valid YAML,
  so editors highlight it, but Otto parses only this form.
- The reader accepts any JSON formatting, RFC 3339 timestamps, and a
  `source` object that omits fields (they default to empty). The writer emits
  the canonical encoding of `memory::codec` (the former `labels_json`,
  `metadata_json` and `source_json` column forms and 30-byte timestamps), so
  a file Otto wrote round-trips byte for byte.
- The body is the record text, one line: `validate_record` rejects control
  characters, including line breaks. The writer appends one `\n`; the reader
  removes one trailing `\n` if present. A body with a second line is invalid.
- The record id is the file name without `.md`.

Tombstone and candidate files are one JSON object each, holding the
`Tombstone` and `Candidate` fields. A decided candidate keeps today's blanked
`proposed` content.

## Reading and hand edits

- Reads take no lock. `get` reads one file; `list`, `retrieve`, standing reads
  and duplicate checks read every file in `records/`.
- Each record passes the existing `validate_record` and the content guard. A
  file that fails parsing, validation or the guard is skipped for reads. A
  write that targets it returns `Corrupt`. Memory open writes one
  `warning: memory file skipped: <path>: <reason>` line to stderr per skipped
  file, the channel of the existing "memory store unavailable" warning, and
  `otto memory status` lists every skipped file with its reason. A file
  broken during a session is reported at the next open or status.
- Only `*.md` in `records/` and `*.json` in `tombstones/` and `candidates/`
  whose names do not start with `.` are read; other entries, such as editor
  backups and in-progress temporary files, are ignored.
- A record whose id also has a tombstone is treated as forgotten.
- When two active records share `(scope, kind, key)` with a non-empty key,
  the one with the later `updated_at` (then the larger id) is used and the
  other is skipped and reported.
- Hand edits to text, kind, key, labels, scope, confidence, expiry and
  metadata take effect at the next read, normally the next turn's recall.
  Otto does not require the editor to change `revision` or `updated_at`.
- Deleting a record file removes the record without a tombstone.
- A new hand-written file must be complete and valid; `/remember` remains the
  supported way to add a record.

The `bash` sandbox cannot write `~/.otto`, and the file tools reject paths
outside the workspace, so the model cannot edit these files unless the
sandbox is `off`. This matches the current exposure of the database file.

## Writes and concurrency

- Every write holds an exclusive `flock` on `.lock`. Acquisition retries for
  `[memory.turso].busy_timeout` (default 5 s), then returns `Busy`.
- Under the lock, the write re-reads the files it depends on and applies the
  existing checks: expected revision (`Conflict`), standing budget
  (`InvalidRequest`), unique active key (`Conflict`), pending candidate state
  (`Conflict`), tombstoned target (`Conflict`).
- Each file is written to a temporary file in the same directory, synced,
  renamed into place, and the directory is synced. The existing
  `reflection::skillwrite` helper does this for skill files and is reused.
- `forget` writes the tombstone, then removes the record file.
- Accepting a candidate writes the record, then rewrites the candidate as
  decided.
- `propose` writes one candidate file per candidate after validating the whole
  batch.

`StoreIdentity.generation` is removed. List and candidate cursors keep their
keyset (`updated_at` or `created_at`, then id) and fingerprint and lose the
generation check. A retrieval cursor keeps its ordinal and the id check at
that ordinal (`InvalidCursor`).

## Recall and search

- Standing preferences are unchanged: user-scope `preference` and
  `instruction` records with a key, oldest first, under their 2000-token
  budget.
- Recall then takes every other active, unexpired record in the session's
  user and workspace scopes. A workspace record replaces a user record with
  the same `(kind, key)`, identical text is de-duplicated as today, and the
  rest is ordered workspace records first, then `updated_at` descending, then
  id, until `max_results` and `recall_tokens`. The message text is not used;
  the service passes an empty query to the store.
- `memory_search` and `/memory search` with a non-empty query split it on
  whitespace (at most 64 terms). A record matches when any term is a
  case-insensitive substring of its text, kind, key or one of its labels.
  Matches are ordered workspace records first, then by the number of matching
  terms, then `updated_at` descending, then id. An empty query lists records
  as today.

Token cost: today a non-standing record enters a request only on an FTS
match. After this change a workspace with records adds up to
`recall_tokens` (default 2000) input tokens to every turn. Lowering
`recall_tokens` reduces it.

## Import from the Turso database

- Memory open imports when `identity.json` is absent and the database that
  the previous release would open exists: the endpoint of a published local
  `memory` route if there is one (`Storage::memory_endpoint`), otherwise
  `[memory.turso].path`. A published route to an external backend fails the
  open with a message to transfer memory back with a 0.4.8 build.
- The database is opened read-only through Turso and must have memory schema
  version 2. Any other version fails the open with a message naming the
  0.4.8 `otto storage migrate` path.
- `memory_meta` supplies `database_id` and `user_scope_id`. Active
  `memory_records` rows become record files, tombstone rows become tombstone
  files, and every `memory_candidates` row becomes a candidate file.
  `memory_observations` is not read; production code never writes it.
- Ids, revisions, timestamps, candidate `created_at`, `base_revision` and
  `proposed` content are preserved. Approval subjects in `approvals.db` are
  derived from `user_scope_id`, the candidate id and a digest of
  `created_at:base_revision:proposed`, so pending approvals stay attached.
- Every row is validated before any file is written. A failing row fails the
  import with its id, and no file is written. `[memory].required` then
  decides between a startup error and a disabled memory service, as for any
  open failure today.
- Files are written under the lock, `identity.json` last. A crash before
  `identity.json` is written repeats the import at the next start; it rewrites
  the same files.
- The database file is never modified or deleted. After the import Otto does
  not read it again. Removing it is manual.
- All Otto processes must be stopped before the first start of this version,
  or writes made to the database by an older process after the import are not
  in the files.

With no database and no `identity.json`, the store creates `identity.json`
with two new ids, as the database does today.

## Configuration compatibility

- `[memory].backend` defaults to `files`. `turso` and `sqlite` are accepted
  with the same meaning; other values are rejected as today.
- `[memory.turso].path` is only the import source. `[memory.turso].busy_timeout`
  is the writer lock wait.
- `require_encryption = true` still fails at startup; files are not encrypted
  at rest.
- `[storage.routes].memory` in user or managed configuration is a
  configuration error that names the key.
- A published route group that contains a `memory` entry still opens for the
  other domains. The entry is read once, as the import source, and otherwise
  ignored.

## Removed

- The Turso memory backend (`storage/backends/turso/memory/`) except the
  row decoding the importer needs, the `memory` storage domain, its
  repository factory, conformance cases, migration codec, `MemoryCodec` FTS
  rebuild, and the FTS storage capability (memory is its only user).
- `memory` in `storage transfer` (including `--memory-db`), `storage init`,
  `storage migrate`, and the approval-to-memory reference checks in
  `storage/migration/references.rs` and
  `storage/backends/migration_references.rs`. `~/.otto/memory/` stays in the
  source home during a transfer, and a live memory approval is transferred
  without a cross-check. A transfer whose published route group contains a
  `memory` entry is refused by name: the cutover requires the frozen and
  published groups to name the same domains as the exported data.
- After the first import (`identity.json` exists), `otto memory` commands do
  not open the storage route authority, so they work while it is frozen.
  Before it, they open the authority to find a published import source.

Unchanged: the `MemoryStore` trait (one implementation, now `FileStore`),
`memory::Service`, the approval flow, reflection, memory advice
(`memory-advice.db`), the `MemoryService` extension path, tool and command
names and outputs.

## Limits

- A crash between the two renames of an accepted create leaves the record
  written and the candidate pending. Accepting it again writes a second
  record with the same text; recall de-duplicates identical text.
- A crash between writing a tombstone and removing the record file leaves the
  file in place; reads treat it as forgotten, and opening the store removes
  it.
- A crash during `propose` can leave part of a batch.
- Revision checks compare revision numbers only. A model update read before a
  hand edit that did not change `revision` overwrites the edit.
- Every recall reads all record files. Record text is at most 8192 bytes; the
  local data above is 41 files and about 6 KB of text in the largest scope.
  An index is the upgrade path if record counts grow to thousands.
- Workspace scope ids are `sha256:` digests; files do not name the workspace
  path. `otto memory list` shows the current workspace's records.

## Tests

- `FileStore`: write-then-read byte round trip; stale revision returns
  `Conflict` and leaves the file unchanged; standing budget; forget writes a
  tombstone and removes the record; a leftover record with a tombstone is not
  read and is removed at open; an invalid file is skipped and reported;
  duplicate key keeps the newer record; a hand edit is visible on the next
  read; two store instances serialize writes through the lock; workspace
  scopes do not leak.
- Recall returns in-scope records newest first within the budget for a
  message that shares no word with them; search matches a CJK term.
- Import: a Turso database with v2 memory tables imports records,
  tombstones, a pending candidate and identity; a second open does not import
  again; the database bytes are unchanged; a non-v2 database fails with the
  migration message.
- Existing service, wiring, CLI, approval and reflection tests move from the
  Turso test support to a temporary store directory.

## Documentation

`docs/manual/memory-and-extensions.md` (memory store, storage migration,
transfer and routes), `docs/manual/servers-and-connectors.md`,
`docs/manual/automation.md`, `docs/development/architecture.md`,
`docs/code-organization.md`, `docs/development/testing.md`, and a
`CHANGELOG.md` entry under "Usage and compatibility" with the migration
steps. The `memory_search` tool description and its `query` parameter
description (`tool/memory.rs`) state that terms are matched as
case-insensitive substrings.

## Not in scope

`memory-advice.db`, `reflection.db` and `approvals.db` stay databases. No
semantic or ranked search. No workspace path in record files.
