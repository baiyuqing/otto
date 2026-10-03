# Turso storage replacement

Status: approved and implemented on 2026-10-03. Full macOS `make check`
passed; Linux execution remains pending CI (no running local Docker daemon).

## Scope

Replace the native executable's SQLite engine and rusqlite calls with the
Turso Rust SDK for memory, usage, subagent task records, workflows,
reflection storage, and skill checks. Use local embedded databases; cloud
sync and remote services are outside this change. The SDK is pinned to
Turso 0.8.1 with native FTS enabled. Keep native dependencies
out of otto-core and otto-web.

## Contracts

Preserve public commands, HTTP/ACP contracts, record identities, permissions,
scope filters, candidate review, append-only usage/events, and workflow
recovery semantics. Session JSONL remains append-only. Adapt database I/O
at the owning native services, preserving cancellation and transaction
ownership; do not introduce a pluggable backend framework.

Replace memory's FTS5 table with a Turso FTS index. Retain field importance
(text 1, kind 0.5, semantic key 2, labels 1), literal-only user queries,
scope isolation, replacement rules, deterministic tie breaking, and token
budgets. Tokenization and BM25 scores may differ from SQLite unicode61;
exact ranking equivalence is not promised. Verify Unicode, punctuation,
injection-like input, updates, deletes, rollback, and reopen behavior.

## Existing databases

Provide an explicit offline migration command: require all Otto writers to
be stopped, preserve original files, copy ordinary records into fresh Turso
databases, rebuild search indexes, and validate before publishing the new
files. Include all database families above. Migration failure must leave
the originals usable. Otto links no SQLite dependency. An embedded Python 3
exporter reads legacy SQLite files only for the explicit offline migration
command. Memory v1
requires migration; ordinary compatible databases can be opened by Turso
directly. Never silently replace a legacy database with an empty store.
Do not migrate the user's actual databases during development.

## Concurrency and acceptance

Verify the selected SDK's multi-process behavior on macOS and Linux,
including lock contention, readers/writers, timeout, rollback, crash/reopen,
and committed-data durability. Existing multi-process access must remain
supported; do not silently impose a single-process restriction. If the
selected release cannot satisfy this contract, report the blocker before
changing the contract.

Run focused storage and migration tests, make check-fast, and make check.
Validate Linux through the available Linux gate or CI and distinguish it
from host validation. Update canonical user/development documentation and
architecture guards with the implementation. No installation or live-data
mutation is included.

## References

- https://github.com/tursodatabase/turso/blob/main/COMPAT.md
- https://github.com/tursodatabase/turso/tree/main/bindings/rust
- https://github.com/tursodatabase/turso/blob/main/docs/fts.md
