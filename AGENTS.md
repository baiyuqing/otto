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

If you introduce tests, add a `test` script in `package.json` and document it here.

## Coding Style & Naming Conventions

Follow TypeScript conventions unless a file dictates otherwise.

- 2-space indentation and UTF-8 files.
- `camelCase` for functions and variables.
- `PascalCase` for types and classes.
- Keep CLI argument names kebab-case (for example `--conversation-log`).

If you add a formatter or linter (for example `prettier` or `eslint`), record the exact commands here.

## Testing Guidelines

No automated tests are configured yet. If you add tests:

- Place them under `tests/`.
- Mirror the `scripts/` layout for integration tests.
- Document how to run them in this file.

## Commit & Pull Request Guidelines

Recent commits are short and direct (for example `Add pnpm lockfile`, `Update trace svg args`). Keep messages concise and imperative.

For pull requests, include:

- A short summary and motivation.
- Test results or manual verification steps.
- Screenshots or logs if the SVG output changes.

## Security & Configuration Notes

`.env` files are ignored. Keep secrets in environment variables. Document any new config files added under `docs/`.
