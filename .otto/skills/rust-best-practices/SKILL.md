---
name: rust-best-practices
description: >-
  Apply Otto-specific idiomatic Rust guidance when writing, reviewing, refactoring,
  debugging, or documenting Rust code in this repository. Covers ownership, APIs,
  errors, async, unsafe code, tests, docs, and validation. Repository AGENTS.md rules
  and established contracts take precedence; this skill does not authorize unrelated
  cleanup.
---

# Rust best practices for Otto

Use this skill as a decision checklist for Rust work, not as a reason to rewrite
working code. It is adapted from BeautifulRust's `rust-best-practices` skill; read
[`references/sources.md`](references/sources.md) when a rule needs its original
reasoning or when the sources disagree.

## Repository precedence

1. Read the root `AGENTS.md`, every scoped `AGENTS.md` on the edited path, and the
   relevant ownership entry in `docs/code-organization.md` before changing code.
2. Repository contracts, established domain terminology, compatibility promises, and
   focused verification commands override generic style advice. Do not rename Otto's
   established `Service` types or rearrange its domain modules solely for style.
3. Preserve behavior and append-only formats unless the task explicitly changes them.
   Pin unclear behavior with a test before refactoring it.
4. Keep the requested scope small. Do not add dependencies, lint policy, public API
   changes, or unrelated cleanup merely because a general guide recommends them.
5. Distinguish library surfaces from application internals. `otto-core` contracts and
   `otto::embed` need stronger API stability; native implementation code should favor
   concrete, direct designs.

## Working loop

1. Locate the owning layer and existing tests.
2. State the behavior and invariants that must remain true.
3. Use the type system to remove invalid states or ambiguous inputs when that makes the
   contract clearer without adding ceremony.
4. Make the smallest complete change, including tests and canonical docs when behavior
   changes.
5. Run `cargo fmt --all` and focused tests. Run broader Make targets only when the task
   or repository instructions require them.

## Ownership and borrowing

- Prefer unique ownership. Use `Arc`, `Rc`, locks, or interior mutability only when
  sharing or mutation is part of the design.
- Do not clone merely to silence the borrow checker. First shorten the borrow, split a
  long expression, use `mem::take`, `mem::replace`, or `Option::take`, or split state
  along real ownership boundaries.
- Take `&str`, `&[T]`, `&Path`, or `&T` for read-only inputs instead of `&String`,
  `&Vec<T>`, `&PathBuf`, or `&Box<T>`. Take `T` when the callee stores or consumes it.
- Prefer owned fields in long-lived structs. Lifetimes on stored references should
  reflect a real borrowing relationship, not an avoided allocation.
- Keep ownership wrappers out of public signatures unless the wrapper is itself part of
  the contract.

## Types and APIs

- Make invalid states unrepresentable when the distinction is stable and meaningful:
  use an enum for named modes, `Option` for absence, and a validated newtype for an
  invariant. Do not replace every local boolean with a type; private obvious flags can
  stay simple.
- Avoid multiple or ambiguous boolean parameters in shared APIs. Prefer an enum or
  separate methods so call sites explain themselves.
- Keep fields private unless the type is deliberately passive data. Start with the
  narrowest useful visibility.
- Use standard traits and vocabulary: `From`/`TryFrom`, `FromStr`, `Display`, `Error`,
  `iter`, `is_empty` with `len`, and `as_`/`to_`/`into_` according to cost and ownership.
- Derive `Debug` and applicable equality, ordering, hashing, cloning, and default traits
  when their semantics are correct. Secret-bearing values need redacted `Debug` output
  with a regression test.
- If equality, ordering, and hashing ignore fields, implement them consistently and
  document the semantics.
- Prefer concrete types, then generics over a narrow trait, then `dyn Trait` at a real
  runtime boundary. Add an abstraction only for an actual consumer boundary.
- Keep public items reachable through one canonical path. Avoid leaking dependency
  types unless the boundary requires them.

## Errors and panics

- Use `Option` when absence is normal, `Result` when callers can act on failure, and a
  panic only for a bug or violated internal invariant.
- Never panic on untrusted input or an expected operational failure. Pattern-match
  rather than check and then unwrap.
- Public error types implement `Debug`, `Display`, and `std::error::Error`; preserve
  useful causes. Application boundaries may use one contextual error type, while shared
  contracts keep specific typed errors.
- Intentional `expect`, `assert`, and `panic` messages explain the invariant and include
  useful values. Follow an existing domain's lock-poison policy instead of changing it
  as incidental cleanup.
- Destructors are best-effort, non-blocking, and infallible. Provide an explicit
  consuming close method when teardown must report failure.

## Everyday code

- Prefer short iterator or `Option`/`Result` pipelines when they read top-to-bottom;
  keep a `for` loop or `match` when it is clearer.
- Use `format!`, `write!`, or `push_str` for string construction rather than chained
  `+`. Use structured project logging rather than `println!` or `dbg!` outside a CLI's
  own output.
- Keep imports explicit and searchable. Glob imports are acceptable only where the
  repository already permits them, commonly small test modules.
- Give constants and identifiers domain names that are easy to grep. Explain the source
  and impact of non-obvious fixed values.

## Async and concurrency

- Prefer message passing. When state must be shared, keep linked invariants under one
  lock, make lock scopes short, and do not call user code or await while holding a lock.
- Public runtime types and returned futures should remain `Send` when callers require
  it; use a compile-time assertion for important boundaries.
- Use `async fn` for ordinary async work. Move blocking work to the repository's
  blocking boundary and make long CPU work yield.
- Libraries do not silently choose a runtime or spawn unmanaged threads.

## Unsafe and platform code

- Use `unsafe` only for a necessary platform/FFI boundary or a measured need that safe
  code cannot express. Keep it in the smallest module with a safe interface.
- Every unsafe block or impl has a `SAFETY:` comment describing why its invariants hold;
  every unsafe function documents caller obligations under `# Safety`.
- Do not use `transmute`, lifetime tricks, or `unsafe impl Send` to bypass the type
  system. Test unsafe abstractions adversarially and with Miri when applicable.
- Keep platform-specific behavior behind `cfg` and limit claims to platforms actually
  tested. Otto's Seatbelt behavior requires its documented macOS acceptance environment.

## Crates, dependencies, and modules

- Respect Otto's existing two-crate boundary: portable I/O-free contracts in
  `otto-core`, native implementations in `otto`. Do not create a crate or module solely
  to satisfy a generic layout rule.
- Follow the repository's exact dependency pins and workspace dependency declarations.
  Review license, build, binary-size, and supply-chain cost before adding a crate.
- Features are additive and named for the capability they enable.
- Keep code within repository file-size and naming limits; split by domain before a file
  crosses the limit.

## Documentation and tests

- Document public contracts where the signature is insufficient: errors, panics,
  safety, side effects, ownership transfer, compatibility, and security boundaries.
  Do not narrate the implementation or restate the name.
- Use `?` rather than `unwrap()` in copyable doc examples.
- Test observable behavior and properties, not duplicated constants or the exact shape
  of the implementation. Bug fixes need a regression test that fails without the fix.
- Follow Otto's test-file placement rule. Keep default tests offline and mock I/O, time,
  randomness, and network behavior at the owning boundary.
- Optimize only after measurement. Preserve readable ownership and APIs until evidence
  identifies a hot path.

## Handback checklist

- The owning layer and scoped `AGENTS.md` rules were followed.
- The change is focused and preserves compatibility outside the requested behavior.
- Input and trust boundaries return errors instead of panicking.
- Shared APIs use clear domain types; clones and synchronization have a design reason.
- Unsafe code, secrets, and platform claims have the required evidence.
- `cargo fmt --all` and focused changed-behavior tests passed, or failures are reported.
- Documentation and `CHANGELOG.md` were updated only when the resulting behavior is
  notable under repository rules.

## Attribution

Adapted from `morganlinton/BeautifulRust` at commit
`c6f56d27a01739d404da2a554daaea023d7ddcca`, licensed under MIT. The retained source
map and upstream license are in `references/sources.md` and `LICENSE`.
