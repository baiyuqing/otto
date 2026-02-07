# Repository Guidelines

## Project Structure & Module Organization

This repo hosts the Agent Trace Toolkit plus a Codex skill definition.

- `scripts/` contains the TypeScript CLIs (`trace_watch.ts`, `trace_svg.ts`).
- `docs/` contains usage docs and the trace log (`agent-trace.md`).
- `skills/agent-trace/` contains the Codex skill (`SKILL.md`, `agents/openai.yaml`).
- `package.json` and `tsconfig.json` define the Node/TypeScript setup.

If you add new top-level directories, document them here to keep the layout discoverable.

## Build, Test, and Development Commands

This project is Node-based and runs TypeScript with `tsx`.

- `npm install` or `pnpm install` installs dependencies.
- `npm run trace:watch -- --conversation-log /path/to/conversation.jsonl` starts the watcher.
- `npm run trace:svg -- --log docs/agent-trace.md --out docs/agent-trace.svg` renders the SVG.
- `npm run trace:ui -- --log docs/agent-trace.md --out docs/agent-trace.html` renders the interactive UI.
- `npm run codex:log` starts Codex with JSONL conversation capture.
- `npm run test` runs the test suite.

Current tests live under `tests/` and are executed by `npm test`.

## Coding Style & Naming Conventions

Follow TypeScript conventions unless a file dictates otherwise.

- 2-space indentation and UTF-8 files.
- `camelCase` for functions and variables.
- `PascalCase` for types and classes.
- Keep CLI argument names kebab-case (for example `--conversation-log`).

If you add a formatter or linter (for example `prettier` or `eslint`), record the exact commands here.

## Testing Guidelines

Automated tests are configured with Node test runner. Existing files:

- `tests/trace_watch.test.mjs`
- `tests/trace_ui.test.mjs`
- `tests/trace_svg.test.mjs`
- `tests/e2e.mjs`

When adding tests:

- Place them under `tests/`.
- Mirror the `scripts/` layout for integration tests.
- Keep tests deterministic across environments.

## Commit & Pull Request Guidelines

Recent commits are short and direct (for example `Add pnpm lockfile`, `Update trace svg args`). Keep messages concise and imperative.

For pull requests, include:

- A short summary and motivation.
- Test results or manual verification steps.
- Screenshots or logs if the SVG output changes.

## Security & Configuration Notes

`.env` files are ignored. Keep secrets in environment variables. Document any new config files added under `docs/`.
