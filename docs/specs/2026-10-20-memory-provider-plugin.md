# Plugin memory: protocol and compatibility decisions

Status: implemented. Approved 2026-10-20. This note retains the wire-compatibility
and authority rationale, not the old implementation plan. Current configuration,
commands, failure recovery and observation limits live in the
[plugin memory manual](../manual/memory-and-extensions.md#plugin-memory-source).
The Rust service contract is described in the
[Open Memory note](2026-10-07-open-memory-contract.md).

## One source, not an extra hook-based memory

One source is active at a time. Selecting `plugin:<package>` replaces built-in
file memory; `enabled = false` disables all memory sources. Installing or
trusting a package alone never selects it. The package declares a memory
component; Otto must not infer that role from its name. External service wire
protocols, tenant membership and learning algorithms belong to the plugin, not
a special Otto `remote` backend.

General hooks and custom tools can still coexist with file memory. They are not
implicitly routed through the selected memory source. The former design in
which an external memory necessarily ran beside built-in memory was superseded.

## Version-one command contract

Each operation runs the declared package command once under the existing
sandbox, digest-bound trust, package credential bindings and redaction rules.
Input is one JSON object with `version: 1`, an `operation`, and (except for
`describe`) a `request`. Output is `{"result": ...}` or
`{"error": {"kind": ...}}`.

`request` and `result` use the serde forms of
`otto_core::memory::memory_types`; `describe` returns `ServiceDescription`.
These types are therefore also a plugin wire contract: preserve their JSON
compatibility or change the protocol version. Fixed error kinds cross the
boundary, not arbitrary plugin error text. Receipt, scope and content checks
still apply. Timeout/cancellation after an effectful command starts is an
unknown outcome, not proof that nothing happened.

## Authority and downgrade boundary

Selecting the source in user/managed configuration grants delegated learning
for the supported user/workspace scopes and input classes. Package execution
trust remains separately required. The service owns resulting changes; Otto
must not fabricate a local review queue or silently fall back to files. Missing
optional memory yields a warning and no memory; required memory fails runner
construction. The manual owns the exact supported consumer matrix.

The parsed registry uses strict component fields. A package installed with a
`memory` component is not readable by a binary predating that component.
Before such a downgrade, remove that package with the newer binary. No session
history migration is needed. Configuration without an explicit backend keeps
file memory; removed `remote` settings fail with plugin migration guidance.
