# Repository Guidelines

## Project Structure & Module Organization

This repository is currently a minimal skeleton. The only tracked file is `.gitignore`, and there are no committed source, test, or asset directories yet. Until the codebase is established, organize new work in conventional Python paths:

- `src/` for application/library code.
- `tests/` for unit/integration tests (mirroring `src/`).
- `scripts/` for one-off utilities and maintenance tasks.
- `docs/` for longer-form documentation.

If you add a new top-level directory, note it in this guide so future contributors can find it.

## Build, Test, and Development Commands

There are no repo-defined build or run scripts yet. When introducing them, add the exact commands here. For now, typical local flows are:

- `python -m venv .venv` to create a local virtual environment.
- `source .venv/bin/activate` to activate it.
- `python -m pytest` to run tests once a test suite exists.

## Coding Style & Naming Conventions

No formatter or linter is configured yet. Keep style consistent with standard Python conventions:

- 4-space indentation and UTF-8 files.
- `snake_case` for functions and variables.
- `PascalCase` for classes.
- Module names in `lower_snake_case`.

If you adopt tools like `black`, `ruff`, or `isort`, document the exact command (for example `python -m black src tests`).

## Testing Guidelines

There are no tests committed yet, but the repository already ignores `.pytest_cache`, implying `pytest` is intended. When adding tests:

- Use filenames like `tests/test_<module>.py`.
- Prefer small, focused tests with clear arrange/act/assert structure.
- Record any coverage targets here if required.

Run the suite with `python -m pytest`.

## Commit & Pull Request Guidelines

Recent commits are short and direct (examples: `Implement`, `Update CLAUDE`, `Add development plan`). Follow that convention: concise, imperative messages under ~50 characters.

For pull requests, include:

- A short summary and the motivation for the change.
- Test results (for example: `python -m pytest`).
- Screenshots or logs when behavior changes are user-visible.

## Security & Configuration Notes

`.env` files are ignored by default. Keep secrets in environment variables and avoid committing credentials. If new configuration files are added, ensure they are documented and safe for version control.
