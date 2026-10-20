import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const root = fileURLToPath(new URL('..', import.meta.url));
const available = spawnSync('gitleaks', ['version']).status === 0;

test('hook blocks staged secrets and history scan detects secrets deleted later', {
  skip: !available && !process.env.GITLEAKS_TEST_REQUIRED,
}, () => {
  assert.ok(available, 'Gitleaks must be installed when GITLEAKS_TEST_REQUIRED is set');
  const dir = mkdtempSync(join(tmpdir(), 'otto-gitleaks-'));
  const run = (cmd, args, env = process.env) => spawnSync(cmd, args, { cwd: dir, env, encoding: 'utf8' });
  const ok = (cmd, args) => {
    const result = run(cmd, args);
    assert.equal(result.status, 0, result.stderr + result.stdout);
  };
  const commit = () => run('git', ['commit', '-qm', 'Controlled scanner test']);
  const key = `AKIA${Array.from({ length: 16 }, (_, i) => String.fromCharCode(65 + i)).join('')}`; // Synthetic AWS-format test input.
  try {
    ok('git', ['init', '-q']);
    ok('git', ['config', 'user.name', 'Scanner test']);
    ok('git', ['config', 'user.email', 'scanner@example.test']);
    ok('git', ['config', 'commit.gpgsign', 'false']);
    mkdirSync(join(dir, 'scripts'));
    writeFileSync(join(dir, 'scripts/pre-commit'), readFileSync(join(root, 'scripts/pre-commit')));
    writeFileSync(join(dir, 'scripts/code-limits.mjs'), readFileSync(join(root, 'scripts/code-limits.mjs')));
    const setup = ['-f', join(root, 'Makefile'), 'setup-git-hooks'];
    ok('make', setup);
    ok('make', setup); // Installation is idempotent.
    writeFileSync(join(dir, 'example.md'), 'Ordinary documentation.\n');
    ok('git', ['add', 'example.md']);
    writeFileSync(join(dir, 'example.md'), `Unstaged credential: ${key}\n`);
    assert.equal(commit().status, 0, 'only staged content is scanned');

    writeFileSync(join(dir, 'example.md'), `Credential: ${key} # gitleaks:allow\n`);
    ok('git', ['add', 'example.md']);
    const blocked = commit();
    assert.notEqual(blocked.status, 0, 'a staged secret must block the actual Git commit');
    assert.match(blocked.stderr + blocked.stdout, /aws-access-token/);
    assert.ok(!(blocked.stderr + blocked.stdout).includes(key), 'output must redact the test credential');

    // Deliberately bypass the hook in this disposable repository to test history.
    ok('git', ['-c', 'core.hooksPath=/dev/null', 'commit', '-qm', 'Controlled secret fixture']);
    writeFileSync(join(dir, 'example.md'), 'Credential removed.\n');
    ok('git', ['add', 'example.md']);
    assert.equal(commit().status, 0);
    const history = run('gitleaks', ['git', '--redact', '--ignore-gitleaks-allow']);
    assert.equal(history.status, 1, 'deletion must not hide a secret in history');

    const missing = run('/bin/sh', [join(dir, '.git/hooks/pre-commit')], { ...process.env, PATH: dir });
    assert.notEqual(missing.status, 0, 'a missing scanner must fail closed');
    const custom = '#!/bin/sh\nexit 0\n';
    writeFileSync(join(dir, '.git/hooks/pre-commit'), custom);
    assert.notEqual(run('make', setup).status, 0, 'do not overwrite an existing different hook');
    assert.equal(readFileSync(join(dir, '.git/hooks/pre-commit'), 'utf8'), custom);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
