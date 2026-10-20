import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { cfgRequiresTest, moduleName, testModuleViolations, violations } from './code-limits.mjs';

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

test('a cfg requires test only when every way to satisfy it sets test', () => {
  for (const predicate of ['test', 'all(test, unix)', 'all(unix, test)', 'any(test, all(test, unix))']) assert.ok(cfgRequiresTest(predicate), predicate);
  for (const predicate of ['unix', 'not(test)', 'any(test, feature = "fault-injection")', 'feature = "test"', 'all(unix, not(test))']) {
    assert.ok(!cfgRequiresTest(predicate), predicate);
  }
});

test('a crate source file is named as a test module exactly when it is compiled only under cfg(test)', () => {
  const src = 'crates/x/src';
  const sources = new Map(
    Object.entries({
      'lib.rs': 'mod gate;\n#[cfg(test)]\nmod fixture_tests;\n#[cfg(all(test, unix))]\n/// Test doubles.\npub(crate) mod doubles;\n#[cfg(any(test, feature = "faults"))]\nmod faults;\nmod shared;\n',
      'gate.rs': '#[cfg(test)]\n#[path = "gate_tests.rs"]\nmod tests;\n',
      'gate_tests.rs': '',
      'fixture_tests.rs': 'mod nested;\n',
      'fixture_tests/nested.rs': '',
      'doubles.rs': '',
      'faults.rs': '',
      'shared/mod.rs': '#[cfg(test)]\nmod tests;\nmod probe_tests;\n',
      'shared/tests.rs': '',
      'shared/probe_tests.rs': '',
      'main.rs': '#[cfg(test)]\nmod faults;\n',
    }).map(([path, text]) => [`${src}/${path}`, text]),
  );
  assert.deepEqual(testModuleViolations(sources), [
    `${src}/doubles.rs: compiled only under cfg(test); name the file or a parent directory \`tests\` or \`<domain>_tests\``,
    `${src}/shared/probe_tests.rs: named as a test module but not compiled only under cfg(test)`,
  ]);
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
    mkdirSync(join(dir, 'crates/x/src'), { recursive: true });
    writeFileSync(join(dir, 'crates/x/src/lib.rs'), '#[cfg(test)]\nmod fixture_tests;\n');
    writeFileSync(join(dir, 'crates/x/src/fixture_tests.rs'), '');
    run('git', ['add', 'crates']);
    writeFileSync(join(dir, 'crates/x/src/lib.rs'), 'mod fixture_tests;\n');
    assert.equal(check().status, 0, 'an unstaged parent change is not checked');
    run('git', ['add', 'crates/x/src/lib.rs']);
    const production = check();
    assert.equal(production.status, 1, 'a staged parent change reclassifies an unstaged child');
    assert.match(production.stderr, /fixture_tests\.rs: named as a test module but not compiled only under cfg\(test\)/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
