import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

test('make install builds and installs the chat connector', () => {
  const output = execFileSync('make', ['-n', 'install', 'INSTALL_DIR=/tmp/otto-install-test'], {
    cwd: fileURLToPath(new URL('..', import.meta.url)),
    encoding: 'utf8',
  });
  const build = output.indexOf('go build -o ../target/otto-connect ./cmd/otto-connect');
  const install = output.indexOf('install -m 0755 target/otto-connect "/tmp/otto-install-test/otto-connect"');
  assert.ok(build >= 0, 'install must build otto-connect');
  assert.ok(install > build, 'install must copy the freshly built otto-connect');
});
