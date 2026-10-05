# Changelog

Notable changes to Otto, grouped by their effect on users and contributors.
Newest releases appear first; pending changes live under **Unreleased**.
Maintenance rules are in the [development guide](docs/development.md#changelog).

This log starts with the three changes merged on 2026-10-05 (#328–#330).
Earlier history is available in [merged pull requests](https://github.com/baiyuqing/otto/pulls?q=is%3Apr+is%3Amerged)
and [GitHub releases](https://github.com/baiyuqing/otto/releases).

## Unreleased

### Usage and compatibility

- Local web access requires approval per host unless the host is listed in
  `[web].allow_hosts`. Network deny disables web access. With the `chatgpt`
  provider and no Google or Brave credentials, search uses ChatGPT's hosted
  tool, which does not use Otto's host approvals. See the
  [web tools manual](docs/user-manual.md#web-tools). ([#328](https://github.com/baiyuqing/otto/pull/328))

### Features

- Add `web_fetch` to read web pages as text, with HTML-to-Markdown extraction
  and offset/limit pagination. Add `web_search` with Google, Brave, ChatGPT
  hosted search, or Bing selected by credentials and provider. Both work
  without `bash`, including on Linux. ([#328](https://github.com/baiyuqing/otto/pull/328))

### Fixes

- Reject a BOM-only `edit.old_text` with an error instead of panicking.
  Restrict fuzzy trailing-whitespace matches to line endings so edits do not
  replace unintended text in the middle of a line. ([#329](https://github.com/baiyuqing/otto/pull/329))

### Security

- Local web tools block non-public destinations, check input for secrets on
  a best-effort basis, ignore proxy environment variables, and refuse
  cross-host redirects and HTTPS-to-HTTP downgrades. Local web results are
  treated as untrusted content and excluded from reflection evidence. ([#328](https://github.com/baiyuqing/otto/pull/328))

### Performance

- Compile Turso dependencies at optimization level 2 in development/test
  builds to speed up storage-heavy tests. ([#330](https://github.com/baiyuqing/otto/pull/330))

### Build and CI

- Reuse the workspace test build for connector checks and stop running PTY
  tests twice. Cache third-party Cargo artifacts and versioned WASM tools
  separately; disable incremental compilation and reduce debug information
  in CI. ([#330](https://github.com/baiyuqing/otto/pull/330))
- Use virtual time in lease and retry tests, replace connector test delays
  with explicit synchronization, and wait for process readiness and log
  events to reduce test time and timing flakes. ([#330](https://github.com/baiyuqing/otto/pull/330))
