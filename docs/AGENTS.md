# Documentation

The [repository rules](../AGENTS.md) apply.

- `README.md` is the documentation index. Current user behavior belongs in
  the root README and the topic pages indexed by `user-manual.md`. Describe
  only implemented/tested capabilities; do not present historical designs,
  planned providers or roadmap stages as shipped features.
- `code-organization.md` is the ownership/entry-point map; the topic pages
  indexed by `development.md` own detailed contributor contracts and
  workflows. Agent instructions link to these canonical sources rather than
  copying inventories.
- Design documents are historical rationale unless their status says
  otherwise. Preserve approval status and compatibility decisions when a
  design becomes implementation.
- Keep notable changes in the root changelog using
  [the changelog rules](development/workflow.md#changelog).
- Do not describe `Binding.Observe`, memory
  backup/restore/verify or `allowed-tools` enforcement as working features.
- Documentation-only edits need no new tests. Check links, paths, examples
  and Markdown layout, and keep any behavioral descriptions aligned with
  their owning code/tests.
