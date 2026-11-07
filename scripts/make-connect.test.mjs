import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

for (const target of ['', 'build-output', '/tmp/otto-build-output']) {
  test(`connect-check finds the runtime with CARGO_TARGET_DIR=${target || '(default)'}`, () => {
    const output = execFileSync('make', ['-n', 'connect-check'], {
      cwd: root,
      encoding: 'utf8',
      env: { ...process.env, CARGO_TARGET_DIR: target },
    });
    const binary = resolve(root, target || 'target', 'debug/otto');
    assert.ok(output.includes(`OTTO_BIN="${binary}" go test -race ./...`), output);
    assert.ok(output.includes('cargo test --workspace --no-run'), output);
  });
}
