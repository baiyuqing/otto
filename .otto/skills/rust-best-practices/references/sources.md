# Sources and local decisions

Read a source when its reasoning matters to the current task. The repository skill is
adapted from [BeautifulRust](https://github.com/morganlinton/BeautifulRust/tree/c6f56d27a01739d404da2a554daaea023d7ddcca/skills/rust-best-practices),
commit `c6f56d27a01739d404da2a554daaea023d7ddcca`.

## Source map

- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/): public API
  naming, traits, conversions, error types, documentation, and future compatibility.
- [Effective Rust](https://www.lurklurk.org/effective-rust/): ownership tradeoffs,
  iterators, errors, dependencies, tooling, and borrow-checker tactics.
- [Rust Design Patterns](https://rust-unofficial.github.io/patterns/): concrete idioms
  such as newtypes, RAII guards, builders, `mem::take`, and known anti-patterns.
- [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/):
  large-project API, async, unsafe, workspace, performance, and agent-facing guidance.
- [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html):
  formatting, imports, modules, ownership, and the application/library distinction.

## Otto-local decisions

These choices intentionally override generic upstream recommendations:

- Root and scoped `AGENTS.md` files plus canonical development docs are authoritative.
- Workspace dependency versions stay exact-pinned; do not convert them to ranges.
- Existing domain names such as `Service` and Otto's domain-oriented module layout stay
  unless the task establishes a concrete design problem.
- Rust test files follow Otto's test-only path rule, not a universal inline-test layout.
- Local verification is `cargo fmt --all` plus focused tests; full Make gates run only
  when requested or required by the repository workflow.
- General best practices do not authorize broad lint changes, dependency additions,
  public API churn, or cleanup outside the task.

## License

BeautifulRust's skill text and source digests are MIT licensed. The upstream copyright
and permission notice are reproduced in the sibling `LICENSE` file. The linked source
guides retain their own licenses; follow the original projects for current terms.
