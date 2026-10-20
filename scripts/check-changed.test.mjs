import assert from 'node:assert/strict';
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { delimiter, join } from 'node:path';
import { tmpdir } from 'node:os';
import { execFileSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { plan } from './check-changed.mjs';

const FMT = 'cargo fmt --all -- --check';
const CLIPPY = 'cargo clippy --workspace --all-targets -- -D warnings';
const FULL = 'cargo test --workspace';
const WASM = 'make rust-wasm-check';
const LICENSE = 'make license-check';

test('check-fast runs the selector directly without a worktree lock utility', () => {
  const fixture = mkdtempSync(join(tmpdir(), 'otto-check-fast-'));
  const bin = join(fixture, 'bin');
  const scripts = join(fixture, 'scripts');
  const calls = join(fixture, 'selector-calls');
  mkdirSync(bin);
  mkdirSync(scripts);
  writeFileSync(join(scripts, 'check-changed.mjs'), `
    import { appendFileSync } from 'node:fs';
    appendFileSync(process.env.SELECTOR_CALLS, 'called\\n');
    console.log('selector fixture ran');
  `);
  for (const utility of ['lockf', 'flock']) {
    const stub = join(bin, utility);
    writeFileSync(stub, '#!/bin/sh\nexit 87\n');
    chmodSync(stub, 0o755);
  }

  try {
    const makefile = fileURLToPath(new URL('../Makefile', import.meta.url));
    const output = execFileSync('make', ['-f', makefile, 'check-fast'], {
      cwd: fixture,
      encoding: 'utf8',
      env: {
        ...process.env,
        PATH: `${bin}${delimiter}${process.env.PATH}`,
        SELECTOR_CALLS: calls,
      },
    });
    assert.match(output, /selector fixture ran/);
    assert.equal(readFileSync(calls, 'utf8'), 'called\n');
  } finally {
    rmSync(fixture, { recursive: true, force: true });
  }
});

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
  for (const p of ['crates/otto/tests/binary_harness/mod.rs', 'crates/otto/tests/fixtures/x.json', 'crates/otto/src/lib.rs', 'crates/otto/build.rs', 'crates/otto/Cargo.toml', 'testdata/x.json', 'crates/other/src/a.rs']) {
    assert.ok(plan([p]).steps.includes(FULL), p);
  }
  assert.ok(!plan(['testdata/x.json']).steps.includes(FMT));
});

test('root Cargo files select clippy, wasm check and full tests, no fmt', () => {
  assert.deepEqual(plan(['Cargo.lock']).steps, [CLIPPY, WASM, FULL, LICENSE]);
});

test('documentation needs no step and is not unchecked', () => {
  assert.deepEqual(plan(['docs/x.md', 'README.md', 'LICENSE']), { steps: [], unchecked: [] });
});

test('CI config, Makefile and unknown directories are unchecked, sorted', () => {
  assert.deepEqual(plan(['desktop/a', 'Makefile', '.github/workflows/checks.yml']), {
    steps: [],
    unchecked: ['.github/workflows/checks.yml', 'Makefile', 'desktop/a'],
  });
});

test('markdown fixture under crates follows the module rule', () => {
  assert.deepEqual(plan(['crates/otto/src/skill/x/SKILL.md']).steps, [CLIPPY, 'cargo test --workspace --lib -- skill::']);
});

test('connect and scripts paths', () => {
  assert.deepEqual(plan(['chat-connector/main.go']).steps, ['make connect-check']);
  assert.deepEqual(plan(['scripts/a.mjs']).steps, ['make scripts-test']);
});

test('license check runs for manifests and lock files only', () => {
  for (const p of ['Cargo.lock', 'Cargo.toml', 'crates/otto/Cargo.toml', 'scripts/license-check.mjs']) {
    assert.ok(plan([p]).steps.includes(LICENSE), p);
  }
  for (const p of ['crates/otto/src/tool/x.rs', 'scripts/a.mjs']) assert.ok(!plan([p]).steps.includes(LICENSE), p);
});

test('mixed change keeps the documented order', () => {
  const { steps } = plan(['scripts/a.mjs', 'chat-connector/a.go', 'crates/otto/src/cli/a.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- cli::', 'make connect-check', 'make scripts-test']);
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
