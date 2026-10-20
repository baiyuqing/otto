import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { moduleName, violations } from './code-limits.mjs';

const lines = (n, line = 'x') => `${Array(n).fill(line).join('\n')}\n`;

test('a code file may have 800 lines, not 801', () => {
  assert.deepEqual(violations('src/a.rs', lines(800)), []);
  assert.deepEqual(violations('src/a.rs', lines(801)), ['src/a.rs: 801 lines (limit 800)']);
  assert.deepEqual(violations('src/a.go', lines(800).slice(0, -1)), [], 'a missing final newline adds no line');
});

test('a line may have 200 code points, not 201', () => {
  assert.deepEqual(violations('a.mjs', `${'中'.repeat(200)}\n`), []);
  assert.deepEqual(violations('a.mjs', `ok\n${'x'.repeat(201)}\n`), ['a.mjs:2: 201 characters (limit 200)']);
});

test('files without a code extension are not checked', () => {
  assert.deepEqual(violations('docs/a.md', lines(900, 'x'.repeat(300))), []);
  assert.deepEqual(violations('src/utils.json', ''), []);
});

test('generic file and module names are rejected, domain names are not', () => {
  for (const p of ['src/utils.rs', 'src/types/mod.rs', 'internal/config/config_test.go', 'scripts/helpers.mjs', 'tests/common/mod.rs']) {
    assert.equal(violations(p, '').length, 1, p);
  }
  for (const p of ['src/memory_types.rs', 'src/server/tests.rs', 'src/config/sandbox.rs', 'crates/otto/src/config/mod.rs', 'crates/otto-core/src/config/mod.rs']) {
    assert.deepEqual(violations(p, ''), [], p);
  }
  assert.equal(moduleName('a/b/mod.rs'), 'b');
  assert.equal(moduleName('a/bridge_test.go'), 'bridge');
});

test('--staged checks staged content only', () => {
  const dir = mkdtempSync(join(tmpdir(), 'otto-code-limits-'));
  const run = (cmd, args) => spawnSync(cmd, args, { cwd: dir, encoding: 'utf8' });
  try {
    run('git', ['init', '-q']);
    mkdirSync(join(dir, 'scripts'));
    copyFileSync(fileURLToPath(new URL('./code-limits.mjs', import.meta.url)), join(dir, 'scripts/code-limits.mjs'));
    const check = () => run('node', ['scripts/code-limits.mjs', '--staged']);
    writeFileSync(join(dir, 'a.rs'), lines(801));
    run('git', ['add', 'a.rs']);
    const blocked = check();
    assert.equal(blocked.status, 1);
    assert.match(blocked.stderr, /a\.rs: 801 lines/);
    writeFileSync(join(dir, 'a.rs'), lines(1));
    assert.equal(check().status, 1, 'the unstaged fix is not what gets committed');
    run('git', ['add', 'a.rs']);
    writeFileSync(join(dir, 'a.rs'), lines(801));
    assert.equal(check().status, 0, 'unstaged content is not checked');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
