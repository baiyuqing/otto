# Plugin packages

Follow-up: [Otto hooks and custom tools](2026-10-06-plugins-and-hooks.md) extend
the installed package contract; unsupported legacy host components below remain
unsupported.


Status: approved 2026-10-05 by the user; implementation in progress. Branch `feat/plugins`, worktree `.worktree/plugins`.

## Goal and scope

Make reusable Skills and MCP configurations installable as versioned packages.
Use Agent Plugins 1.0 as the portable baseline rather than inventing an Otto
package format. Plugin support is enabled by default; an empty installation
registry adds no capabilities or network activity.

The first release supports local directories and Git repositories pinned to a
full commit ID, installation inspection, enable/disable, removal, and executable
trust/revocation. It supports portable Skills and MCP stdio/Streamable HTTP.
Legacy Claude and Codex manifests receive a limited Skills/MCP adapter with
explicit unsupported-component diagnostics. This is package/component
compatibility, not full client compatibility.

Out of scope: marketplaces, automatic updates, npm installation, dependency
installation scripts, Commands, Agents, Hooks, LSP, OpenAI Apps, Pi JavaScript
extensions, UI extensions, and live replacement of an active runner.

## Package contract

Prefer root `plugin.json` with the supported Agent Plugins schema version.
Discover `skills/*/SKILL.md` and `mcp.json` according to that specification.
Validate using local rules; never download schemas at startup. Resolve package
boundaries after canonicalization and reject symlink/path escapes. Isolate an
invalid Skill or MCP server while retaining valid sibling components; a fatal
manifest/schema error prevents package activation.

If there is no portable manifest, accept `.codex-plugin/plugin.json` or
`.claude-plugin/plugin.json`. If both exist without a portable manifest, reject
the ambiguity rather than choose silently. Read only their Skills/MCP component
declarations and conventional defaults. Follow each format's path semantics,
but enforce the same package boundary. Report ignored component types and
unsupported fields with their package identity; never execute them indirectly.

Unknown host-extension namespaces in portable packages are ignored as required
by the standard. Recognized declarations of unsupported executable components
are reported. Do not call a mixed package fully compatible simply because its
Skills loaded.

## Installation and identity

Native installation copies a local directory or checks out a pinned Git commit
into a staging directory, validates it, and atomically publishes a snapshot.
Do not install in place, follow escaping symlinks, run repository scripts, or
initialize submodules. Git URLs with embedded credentials are rejected. Installer subprocesses exclude
global/system Git configuration to prevent attribute filters from executing
package-triggered code; SSH agents and `GIT_ASKPASS` authentication remain
available. Configuration override variables are also excluded. Never persist credentials.

The user-owned registry records manifest name/version, source, resolved commit
where applicable, content digest, enabled state, and normalized component
inventory. The content digest covers published paths, bytes and executable
modes. Reject duplicate manifest names; replacement requires an explicit new
installation and cannot silently change an existing package.

Store snapshots under `~/.otto/plugins/packages/<digest>/`, registry state under
`~/.otto/plugins/`, and persistent data under `~/.otto/plugins/data/<name>/`.
Publish snapshots as read-only where supported; permissions supplement digest
checks rather than establish trust. Do not include writable plugin data in the
package digest. Removal disables the package and retains its data; keep any
snapshot used by an active runner until that runner releases it.

Proposed CLI:

```text
otto plugin install <local-directory>
otto plugin install <git-url> --rev <full-commit-id>
otto plugin list
otto plugin inspect <name>
otto plugin enable <name>
otto plugin disable <name>
otto plugin trust <name>
otto plugin revoke <name>
otto plugin remove <name>
```

Installation enables available Skills. MCP stays inactive until executable
trust is granted. `inspect` shows source, digest, components, compatibility
diagnostics and trust status without resolved secrets. `trust` displays the
execution/network configuration being authorized and binds authorization to
the snapshot digest. Installation never grants execution by itself.

## Names and configuration

Give a plugin Skill the catalog/tool lookup ID `<plugin>:<skill>` while
preserving its local frontmatter name and directory-basename validation.
Existing standalone Skill names and override rules remain compatible. Prompt,
load/read dispatch, disabled lists and skill-derived subagent selection use
the qualified ID; do not rewrite upstream `SKILL.md` files.

Give MCP components logical identities `<plugin>:<server>`. Generate bounded,
deterministic internal server names for the existing MCP registry and detect
collisions before connecting. Display the logical identity in diagnostics and
management surfaces. Explicit standalone MCP servers retain their identities.

Add a layered `[plugins]` configuration with `enabled` (default true) and
`disabled` (manifest names). Existing `[skills].enabled` and `[mcp].enabled`
also gate the corresponding plugin components. Project sources may disable
components but cannot supply installation sources, grant executable trust,
inject credentials, or override managed policy. Configuration merging must
preserve source authority and provenance.

Portable MCP interpolation recognizes only the standard `${PLUGIN_ROOT}` and
`${PLUGIN_DATA}` in permitted fields; do not feed manifests through Otto's
general environment expansion. Legacy adapters recognize their documented
package-root placeholders only. Any Otto credential overrides come from
explicit user-owned `[plugins.bindings."<plugin>:<server>"]` environment
references (`env`/`headers`, plus HTTP OAuth options), resolved and redacted through
existing configuration contracts. Do not persist literal secrets in manifests,
registry state or diagnostics. Reject unsupported transport types, including
legacy HTTP+SSE, with a component-specific diagnostic.

## Authorization and lifecycle

Reuse the existing MCP transports, OAuth and project/managed-policy guardrails.
Add package digest authorization rather than treating a global installation as
project executable trust. Neither a manifest nor project configuration can
grant execution or relax policy. MCP stdio retains Otto's documented execution
model; do not claim sandbox confinement that the existing transport lacks.

Check digest/trust before connection and at subsequent MCP effect boundaries.
Revocation closes affected servers and prevents later calls in existing
sessions. A changed snapshot loses authorization. One package/server failure
does not prevent unrelated components from loading. Child environments remain
explicit and use the existing secret-redaction rules.

Runner construction captures installed/enabled package versions and component
IDs. Registry changes take effect on the next runner build, normally a new
session or process restart; there is no mid-turn registry replacement. Record
package provenance as an append-only session metadata entry without changing
existing message semantics. Resuming history must not silently reinstall or
execute packages named in old metadata.

## Ownership

`otto-core` owns pure configuration, manifest/adapter types and validation where
shared consumers need them. It remains WASM-safe. Native `otto::plugin` owns
filesystem discovery, installation, registry mutation, digest/trust state and
package-to-runtime adaptation. Use shared pinned filesystem and atomic-write
operations; serialize concurrent registry mutations.

`runtime::Builder` composes plugin Skills and MCP with existing capabilities.
All CLI/TUI/HTTP/ACP runners use this path. CLI commands call the native service;
the service never imports frontends. Keep the existing Skill catalog, MCP
router, session storage and layered configuration as their owning contracts.

## Acceptance and delivery

- Offline fixtures cover portable packages and the supported legacy subsets.
- Two plugins with the same local Skill/MCP name remain independently callable.
- Malformed components, unsupported declarations and boundary escapes produce
  useful diagnostics; they cannot execute unsupported code.
- Installation invokes no package scripts; Git tests use local fixture repos.
- Enable/disable/removal have the documented runner-boundary behavior, while
  revocation blocks active MCP calls and closes affected servers.
- Changed package content cannot reuse previous executable authorization.
- Credential resolution and diagnostics do not expose environment values.
- Shared runtime integration, configuration authority and native/WASM
  boundaries are covered by focused behavioral tests and architecture checks.
- Run `make rust-wasm-check` for shared changes, then `make check-fast`.
  macOS execution acceptance requires the supported macOS environment.
- Update README, user manual, ownership map and changelog for implemented
  behavior, then open a PR after required checks.

## References

- [Agent Plugins specification](https://agent-plugins.org/specification)
- [Client conformance](https://agent-plugins.org/client-implementers/conformance)
- [Codex plugin construction](https://developers.openai.com/codex/plugins/build)
- [Claude plugin structure](https://github.com/anthropics/claude-code/blob/main/plugins/plugin-dev/skills/plugin-structure/SKILL.md)
- [Pi extensions](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md)
