# External plugins and lifecycle hooks

Status: approved by user on 2026-10-06; implemented, pending PR merge.

Expose plugin packages containing existing skills/MCP definitions, external
command tools and lifecycle hooks. Use argv plus versioned JSON stdin/stdout,
not code loaded into Otto's process. Reuse the installed package format and content trust from PR #356, sandbox, project executable trust,
exact-argument authorization and append-only session history.

Hooks cover session start/end, prompt submission, tool execution, compaction
and turn completion across all native frontends and sub-agents. Before hooks
may deny or supply permitted rewrites; after hooks preserve execution facts.
Parameter rewrites precede policy/validation/authorization. Context is untrusted.
No automatic replay after process loss, marketplace, or in-process extension ABI.

Canonical protocol: [Hooks and custom tools](../user-manual.md#hooks-and-custom-tools).
Implementation invariants: [Plugin execution contracts](../development.md#plugin-execution-contracts).

Integration: retain Agent Plugins `plugin.json` and the installed snapshot registry.
Versioned hook/tool declarations live in `extensions.dev.otto`; no parallel
package loader or management commands are introduced.
