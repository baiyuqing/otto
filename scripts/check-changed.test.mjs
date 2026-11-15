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
  const { steps, unchecked } = plan(['crates/otto/src/memory/store.rs', 'crates/otto/src/app/mod.rs', 'crates/otto/src/memory/x.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- app:: memory::']);
  assert.deepEqual(unchecked, []);
});

test('otto-core change selects full tests, not --lib', () => {
  const { steps } = plan(['crates/otto-core/src/lib.rs', 'crates/otto/src/memory/store.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, FULL]);
});

test('top-level integration test files map to --test', () => {
  const { steps } = plan(['crates/otto/tests/b.rs', 'crates/otto-cli/tests/a.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --test a --test b']);
});

test('modules of the frontend crates map to --lib filters by module name', () => {
  const { steps } = plan(['crates/otto-server/src/turn_routes.rs', 'crates/otto-acp/src/stdio/x.rs', 'crates/otto-cli/src/run/attach.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- run:: stdio:: turn_routes::']);
});

test('modules of otto-eval map to --lib filters by module name', () => {
  const { steps } = plan(['crates/otto-eval/src/memory_eval_command/items.rs', 'crates/otto-eval/src/eval_command.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- eval_command:: memory_eval_command::']);
  assert.ok(plan(['crates/otto-eval/src/lib.rs']).steps.includes(FULL));
});

test('modules of otto-terminal map to --lib filters and otto-client selects full tests', () => {
  const { steps } = plan(['crates/otto-terminal/src/tui/app.rs', 'crates/otto-terminal/src/cli/repl.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- cli:: tui::']);
  assert.ok(plan(['crates/otto-client/src/sessions.rs']).steps.includes(FULL));
});

test('modules of otto-platform map to --lib filters by module name', () => {
  const { steps } = plan(['crates/otto-platform/src/fsops/posix.rs', 'crates/otto-platform/src/log.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- fsops:: log::']);
});

test('modules of otto-storage map to --lib filters by module name', () => {
  const { steps } = plan(['crates/otto-storage/src/worker.rs', 'crates/otto-storage/src/drivers/sqlite.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- drivers:: worker::']);
});

test('module and integration test steps run in that order', () => {
  const { steps } = plan(['crates/otto/tests/a.rs', 'crates/otto/src/skill/mod.rs']);
  assert.deepEqual(steps.slice(2), ['cargo test --workspace --lib -- skill::', 'cargo test --workspace --test a']);
});

test('test support directories and crate entry files select full tests', () => {
  for (const p of [
    'crates/otto/tests/binary_harness/mod.rs',
    'crates/otto/tests/fixtures/x.json',
    'crates/otto-cli/tests/acp_attach/commands.rs',
    'crates/otto/src/fixtures.rs',
    'crates/otto/src/fixtures/scripted_provider.rs',
    'crates/otto/src/lib.rs',
    'crates/otto/build.rs',
    'crates/otto/Cargo.toml',
    'crates/otto-server/src/lib.rs',
    'crates/otto-acp/Cargo.toml',
    'crates/otto-cli/src/main.rs',
    'testdata/x.json',
    'crates/other/src/a.rs',
  ]) {
    assert.ok(plan([p]).steps.includes(FULL), p);
  }
  assert.ok(!plan(['testdata/x.json']).steps.includes(FMT));
});

test('root Cargo files select clippy and full tests, no fmt', () => {
  assert.deepEqual(plan(['Cargo.lock']).steps, [CLIPPY, FULL, LICENSE]);
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
  const { steps } = plan(['scripts/a.mjs', 'chat-connector/a.go', 'crates/otto-cli/src/a.rs']);
  assert.deepEqual(steps, [FMT, CLIPPY, 'cargo test --workspace --lib -- a::', 'make connect-check', 'make scripts-test']);
});

test('every tests/*.rs file of the lib crates maps to --test <name>', () => {
  const crates = new URL('../crates/', import.meta.url);
  let seen = 0;
  for (const crate of ['otto', 'otto-platform', 'otto-storage', 'otto-terminal', 'otto-server', 'otto-acp', 'otto-eval', 'otto-cli']) {
    const dir = join(fileURLToPath(crates), crate, 'tests');
    let names;
    try {
      names = readdirSync(dir).filter((f) => f.endsWith('.rs'));
    } catch {
      continue;
    }
    for (const f of names) {
      seen += 1;
      const { steps } = plan([`crates/${crate}/tests/${f}`]);
      assert.ok(steps.includes(`cargo test --workspace --test ${f.slice(0, -3)}`), `${crate}/${f}`);
    }
  }
  assert.ok(seen > 0);
});
