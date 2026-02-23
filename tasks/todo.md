# Task Plan

## Previous Task

- [x] Review benchmark connection lifecycle and identify insertion points for connection mode.
- [x] Add configurable benchmark modes: long-running shared connection pool mode and single-new-connection-per-transaction mode.
- [x] Add/adjust unit tests for mode parsing and mode behavior guards.
- [x] Update README docs to describe the new mode flag and examples.
- [x] Run test suite and record results.

## Current Task: Reorganize `main.go`

- [x] Confirm refactor scope and keep behavior unchanged.
- [x] Split `main.go` into focused files by concern (config, metrics, runner, reporting, entrypoint).
- [x] Run gofmt and test suite to verify no regressions.
- [x] Review git diff for minimal-impact structure-only changes.
- [x] Record review notes and verification results.

## Review

### Previous Task

- Added a new `-connection-mode` flag with `long-running` (default) and `per-transaction` modes.
- Refactored execution to use a query runner abstraction so worker logic is unchanged while connection lifecycle is mode-dependent.
- Added unit coverage for parsing/validating the connection mode option.
- Updated English and Chinese README files to describe both modes.
- Validation: `go test ./...` passed.

### Current Task

- Reorganized the single-file implementation into focused files while keeping package scope and behavior unchanged.
- Kept the same runtime flow in `main()` and moved parsing, metrics, query execution, and reporting into dedicated files.
- Verified formatting and tests after refactor.
- Validation: `go test ./...` passed.
