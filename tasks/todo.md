# Task Plan

- [x] Review benchmark connection lifecycle and identify insertion points for connection mode.
- [x] Add configurable benchmark modes: long-running shared connection pool mode and single-new-connection-per-transaction mode.
- [x] Add/adjust unit tests for mode parsing and mode behavior guards.
- [x] Update README docs to describe the new mode flag and examples.
- [x] Run test suite and record results.

## Review

- Added a new `-connection-mode` flag with `long-running` (default) and `per-transaction` modes.
- Refactored execution to use a query runner abstraction so worker logic is unchanged while connection lifecycle is mode-dependent.
- Added unit coverage for parsing/validating the connection mode option.
- Updated English and Chinese README files to describe both modes.
- Validation: `go test ./...` passed.
