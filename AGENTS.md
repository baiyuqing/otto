# Repository Guidelines

## Project Structure & Module Organization

This repository is currently minimal. The root contains only `LICENSE` and Git metadata, so keep top-level files limited to documents and configuration.

When adding code, use a predictable layout:

- `src/` for application or library code
- `tests/` for automated tests
- `docs/` for design notes or usage guides
- `assets/` for static files such as sample data or images

Prefer small, focused modules and keep related tests next to the feature area they cover inside `tests/`.

## Build, Test, and Development Commands

This repository now uses a minimal TypeScript toolchain.

- `npm install` installs development dependencies
- `npm run build` compiles the TypeScript source into `dist/`
- `npm run collab:seed-demo -- .otto/collaboration.sqlite` seeds a local SQLite collaboration database with demo DM/channel/agent data
- `npm run company:seed-demo -- .otto/company.sqlite` seeds a local SQLite collaboration database with a 3-agent "small company" demo
- `npm run collab:serve -- .otto/collaboration.sqlite --port 4318` serves the collaboration snapshot over a local read-only HTTP API
- `npm run dev -- --runtime demo "your prompt"` runs the CLI directly from TypeScript
- `npm run dev -- --list-runtimes --remote-runtimes claude,codex,gemini` lists local and remote daemon-discovered runtime targets
- `npm run cli -- --runtime demo "your prompt"` runs the built CLI from `dist/`
- `cd web && npm install && npm run dev` starts the collaboration UI preview locally
- `cd web && npm run build` builds the collaboration UI preview
- `npm run typecheck` runs the TypeScript compiler without emitting files
- `npm test` runs the Vitest test suite
- `git status` checks the working tree before and after changes
- `git log --oneline -n 5` reviews recent commit style and scope
- `ls -la` confirms the repository layout from the root

## Coding Style & Naming Conventions

Match the conventions of the language you introduce and keep formatting automated where possible. Use 4 spaces for indentation in prose examples and Python-style code; use the standard formatter for other ecosystems.

Naming should stay conventional:

- directories and data files: lowercase with hyphens or underscores
- source files: language-idiomatic names
- classes/types: `PascalCase`
- functions and variables: `camelCase` or `snake_case`, depending on the language

## Testing Guidelines

Add tests with every functional change. Place them under `tests/` and name them clearly, such as `test_parser.py`, `parser.test.ts`, or `parser_test.go`.

Run `npm test` before finishing a change. For type-level or structural changes, also run `npm run typecheck`.

Because no coverage tooling exists yet, aim for meaningful coverage of the changed behavior and any critical edge cases. Document the exact command used to run the tests in your PR.

## Commit & Pull Request Guidelines

The current history starts with `Initial commit`, so follow short, imperative commit subjects such as `Add AGENTS contributor guide` or `Create tests for parser`.

Pull requests should include:

- a brief summary of what changed
- any setup, test, or verification commands used
- linked issues, if applicable
- screenshots or sample output when UI or generated artifacts change

## Contributor Notes

Do not add large binaries or generated files unless they are required for the repository’s purpose. If you introduce a new toolchain, keep its configuration minimal and update this guide in the same change.
