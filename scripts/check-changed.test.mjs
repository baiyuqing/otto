import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { plan } from './check-changed.mjs';

const FMT = 'cargo fmt --all -- --check';
const CLIPPY = 'cargo clippy --workspace --all-targets -- -D warnings';
const FULL = 'cargo test --workspace';
const WASM = 'make rust-wasm-check';
const UI = 'make ui ui-test';

test('two modules share one --lib step, no full test', () => {
  const { steps, unchecked } = plan(['crates/otto/src/memory/store.rs', 'crates/otto/src/tui.rs', 'crates/otto/src/memory/x.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- memory:: tui::']);
  assert.deepEqual(unchecked, []);
});

test('otto-core change selects full tests and wasm check, not --lib', () => {
  const { steps } = plan(['crates/otto-core/src/lib.rs', 'crates/otto/src/memory/store.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, WASM, FULL]);
});

test('top-level integration test files map to --test', () => {
  const { steps } = plan(['crates/otto/tests/b.rs', 'crates/otto/tests/a.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --test a --test b']);
});

test('module and integration test steps run in that order', () => {
  const { steps } = plan(['crates/otto/tests/a.rs', 'crates/otto/src/skill/mod.rs']);
  assert.deepEqual(steps.slice(2), ['cargo test --workspace --lib -- skill::', 'cargo test --workspace --test a']);
});

test('test support directories and crate entry files select full tests', () => {
  for (const p of ['crates/otto/tests/common/mod.rs', 'crates/otto/tests/fixtures/x.json', 'crates/otto/src/lib.rs', 'crates/otto/build.rs', 'crates/otto/Cargo.toml', 'testdata/x.json', 'crates/other/src/a.rs']) {
    assert.ok(plan([p]).steps.includes(FULL), p);
  }
  assert.ok(!plan(['testdata/x.json']).steps.includes(FMT));
});

test('root Cargo files select clippy, wasm check and full tests, no fmt', () => {
  assert.deepEqual(plan(['Cargo.lock']).steps, [CLIPPY, WASM, FULL]);
});

test('ui-only change runs only ui build and tests', () => {
  assert.deepEqual(plan(['ui/src/App.tsx']).steps, [UI]);
});

test('otto-web runs wasm tests and the ui, not the native tests', () => {
  const { steps } = plan(['crates/otto-web/src/lib.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, WASM, 'wasm-pack test --node crates/otto-web', UI]);
});

test('documentation needs no step and is not unchecked', () => {
  assert.deepEqual(plan(['docs/x.md', 'README.md', 'LICENSE']), { steps: [], unchecked: [] });
});

test('desktop selects its gate; CI config and Makefile are unchecked, sorted', () => {
  assert.deepEqual(plan(['desktop/a', 'Makefile', '.github/workflows/checks.yml']), {
    steps: ['make desktop-check'],
    unchecked: ['.github/workflows/checks.yml', 'Makefile'],
  });
});

test('markdown fixture under crates follows the module rule', () => {
  assert.deepEqual(plan(['crates/otto/src/skill/x/SKILL.md']).steps, [CLIPPY, 'cargo test --workspace --lib -- skill::']);
});

test('connect and scripts paths', () => {
  assert.deepEqual(plan(['connect/main.go']).steps, ['make connect-check']);
  assert.deepEqual(plan(['scripts/a.mjs']).steps, ['make scripts-test']);
});

test('mixed change keeps the documented order', () => {
  const { steps } = plan(['scripts/a.mjs', 'connect/a.go', 'ui/a.ts', 'crates/otto-web/a.rs', 'crates/otto/src/cli/a.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, WASM, 'cargo test --workspace --lib -- cli::', 'wasm-pack test --node crates/otto-web', UI, 'make connect-check', 'make scripts-test']);
});

test('every crates/otto/tests/*.rs file maps to --test <name>', () => {
  const dir = new URL('../crates/otto/tests/', import.meta.url);
  const names = readdirSync(fileURLToPath(dir)).filter((f) => f.endsWith('.rs'));
  assert.ok(names.length > 0);
  for (const f of names) {
    const { steps } = plan([`crates/otto/tests/${f}`]);
    assert.ok(steps.includes(`cargo test --workspace --test ${f.slice(0, -3)}`), f);
  }
});
