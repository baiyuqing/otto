# Documentation

The [repository rules](../AGENTS.md) apply.

- `README.md` is the documentation index. The root README owns installation,
  first use and navigation; topic pages indexed by `user-manual.md` own detailed
  current behavior. Keep one canonical explanation and link rather than repeat. Describe
  only implemented/tested capabilities; do not present historical designs,
  planned providers or roadmap stages as shipped features.
- `code-organization.md` is the ownership/entry-point map; the topic pages
  indexed by `development.md` own detailed contributor contracts and
  workflows. Agent instructions link to these canonical sources rather than
  copying inventories.
- Keep docs focused on current use and maintenance. Delete superseded designs,
  obsolete migration narratives and duplicate history; Git preserves old text.
  Retain design notes only for still-useful rationale or approval/compatibility
  decisions absent from current contracts. Update surviving references when
  removing a page; do not add historical placeholder pages.
- Keep notable changes in the root changelog using
  [the changelog rules](development/workflow.md#changelog).
- Do not describe `Binding.Observe`, memory
  backup/restore/verify or `allowed-tools` enforcement as working features.
- Documentation-only edits need no new tests. Check links, paths, examples
  and Markdown layout, and keep any behavioral descriptions aligned with
  their owning code/tests.
