# Browser WASM bindings

The [repository rules](../../AGENTS.md) apply.

- This crate exposes `otto-core` wire codecs through `wasm-bindgen`; keep
  application/domain logic in the shared core and native services in `otto`.
- Stay buildable for `wasm32-unknown-unknown`. Keep JavaScript exports and
  TypeScript consumers under `ui/` consistent when a binding changes.
- Run relevant WASM/codec tests and the root `make check-fast`. Binding
  changes also require the browser build/tests selected by that gate.

Use the [development guide](../../docs/development.md#package-boundaries)
for boundaries and the Makefile for canonical build commands.
