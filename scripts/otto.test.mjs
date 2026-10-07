import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const LAUNCHER = fileURLToPath(new URL('./otto.sh', import.meta.url));
const EXAMPLE = fileURLToPath(new URL('../otto.env.example', import.meta.url));
const ROOT = fileURLToPath(new URL('..', import.meta.url));

// A fake Otto that records the arguments and the API key it received, so these
// tests need neither a built binary nor a live provider.
const FAKE_OTTO = `#!/usr/bin/env bash
{
  for arg in "$@"; do printf 'arg=%s\\n' "$arg"; done
  printf 'api_key=%s\\n' "\${OTTO_API_KEY:-}"
} > "\${OTTO_CAPTURE:?}"
`;

function context(t) {
  const dir = mkdtempSync(join(tmpdir(), 'otto-launcher-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const bin = join(dir, 'otto');
  writeFileSync(bin, FAKE_OTTO);
  chmodSync(bin, 0o755);
  const capture = join(dir, 'capture.txt');
  const config = join(dir, 'otto.env');
  const writeConfig = (text) => writeFileSync(config, text);
  const read = () => {
    const lines = readFileSync(capture, 'utf8').split('\n').filter(Boolean);
    return {
      argv: lines.filter((line) => line.startsWith('arg=')).map((line) => line.slice(4)),
      apiKey: lines.filter((line) => line.startsWith('api_key=')).map((line) => line.slice(8))[0] ?? '',
    };
  };
  const run = (args = [], env = {}) =>
    spawnSync('bash', [LAUNCHER, ...args], {
      encoding: 'utf8',
      env: { PATH: process.env.PATH, OTTO_BIN: bin, OTTO_ENV_FILE: config, OTTO_CAPTURE: capture, ...env },
    });
  return { dir, bin, capture, config, writeConfig, read, run };
}

test('turns otto.env settings into the matching Otto options', (t) => {
  const { writeConfig, read, run } = context(t);
  writeConfig(`# Launcher settings.
OTTO_API_KEY=synthetic-launcher-key
OTTO_PROVIDER=openai-compatible
OTTO_MODEL=example-model
OTTO_PROFILE=work
OTTO_BASE_URL=https://example.invalid/v1
OTTO_CWD=/tmp/example-workspace
OTTO_UI=tui
OTTO_SANDBOX=auto
OTTO_THINKING=high
OTTO_SHELL_TIMEOUT=120s
OTTO_MAX_OUTPUT_BYTES=65536
`);
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(read().argv, [
    '--provider', 'openai-compatible',
    '--model', 'example-model',
    '--profile', 'work',
    '--base-url', 'https://example.invalid/v1',
    '--cwd', '/tmp/example-workspace',
    '--ui', 'tui',
    '--sandbox', 'auto',
    '--thinking', 'high',
    '--shell-timeout', '120s',
    '--max-output-bytes', '65536',
  ]);
  assert.equal(read().apiKey, 'synthetic-launcher-key');
});

test('omits empty settings so Otto falls back to its own configuration', (t) => {
  const { writeConfig, read, run } = context(t);
  writeConfig(`OTTO_PROVIDER=
OTTO_MODEL=example-model
OTTO_CWD=
`);
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(read().argv, ['--model', 'example-model']);
});

test('command-line options come last, so they win over the configuration file', (t) => {
  const { writeConfig, read, run } = context(t);
  writeConfig(`OTTO_MODEL=example-model
OTTO_SANDBOX=auto
`);
  const result = run(['--model', 'other-model', '--cwd', '/tmp/other']);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(read().argv, [
    '--model', 'example-model',
    '--sandbox', 'auto',
    '--model', 'other-model',
    '--cwd', '/tmp/other',
  ]);
});

test('splits OTTO_EXTRA_ARGS before the command-line options', (t) => {
  const { writeConfig, read, run } = context(t);
  writeConfig('OTTO_EXTRA_ARGS="--no-session --thinking low"\n');
  const result = run(['--prompt', 'explain the build']);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(read().argv, [
    '--no-session',
    '--thinking', 'low',
    '--prompt', 'explain the build',
  ]);
});

test('keeps quoted settings with spaces in one argument', (t) => {
  const { writeConfig, read, run } = context(t);
  writeConfig('OTTO_CWD="/tmp/example workspace"\n');
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(read().argv, ['--cwd', '/tmp/example workspace']);
});

test('warns about a missing configuration file and still starts Otto', (t) => {
  const { dir, read, run } = context(t);
  const result = run(['--model', 'example-model'], { OTTO_ENV_FILE: join(dir, 'absent.env') });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /absent\.env/);
  assert.match(result.stderr, /otto\.env\.example/);
  assert.deepEqual(read().argv, ['--model', 'example-model']);
});

test('reports a missing Otto binary instead of starting anything', (t) => {
  const { dir, capture, run } = context(t);
  const result = run([], { OTTO_BIN: join(dir, 'not-built') });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /make build/);
  assert.equal(existsSync(capture), false);
});

test('every otto.env.example setting is one a consumer handles', () => {
  // scripts/otto.sh and scripts/otto-tcp.sh read the same file: the launcher,
  // and the ACP TCP stack that turns its settings into `otto serve` options.
  const settings = new Set([
    'OTTO_API_KEY', 'OTTO_PROVIDER', 'OTTO_MODEL', 'OTTO_PROFILE', 'OTTO_BASE_URL',
    'OTTO_CWD', 'OTTO_UI', 'OTTO_SANDBOX', 'OTTO_THINKING', 'OTTO_SHELL_TIMEOUT',
    'OTTO_MAX_OUTPUT_BYTES', 'OTTO_EXTRA_ARGS',
    'OTTO_ACP_LISTEN', 'OTTO_CONNECT_CONFIG', 'OTTO_LISTEN', 'OTTO_SERVE_EXTRA_ARGS',
    'OTTO_TCP_HOME',
  ]);
  const example = readFileSync(EXAMPLE, 'utf8');
  const keys = example.split('\n').filter((line) => /^[A-Z][A-Z0-9_]*=/.test(line)).map((line) => line.split('=')[0]);
  assert.ok(keys.length > 0, 'otto.env.example documents no settings');
  for (const key of keys) assert.ok(settings.has(key), `${key} is documented but no consumer reads it`);
});

test('make run starts the launcher', () => {
  const output = execFileSync('make', ['-n', 'run'], { cwd: ROOT, encoding: 'utf8' });
  assert.match(output, /\.\/scripts\/otto\.sh/);
});

test('otto.env and its variants stay out of Git while the example is committed', () => {
  const ignored = (path) => spawnSync('git', ['check-ignore', '-q', path], { cwd: ROOT }).status;
  // A real key lives in otto.env, so an unignored name would be committed by
  // `git add -A` without anyone noticing.
  for (const path of ['otto.env', 'otto.env.local', 'otto.env.bak']) {
    assert.equal(ignored(path), 0, `${path} must be ignored`);
  }
  assert.equal(ignored('otto.env.example'), 1, 'otto.env.example must stay committable');
  const tracked = execFileSync('git', ['ls-files', 'otto.env.example'], { cwd: ROOT, encoding: 'utf8' }).trim();
  assert.equal(tracked, 'otto.env.example');
});
