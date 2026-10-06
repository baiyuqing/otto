#!/usr/bin/env node
// Opt-in container acceptance: build otto:local first, then run this script.
// No provider requests are made. Every volume/container uses a unique project.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { chmodSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const workspace = mkdtempSync(join(tmpdir(), 'otto-docker-smoke-'));
chmodSync(workspace, 0o777); // Permit container UID 1000 in this disposable fixture.
const project = `otto-smoke-${process.pid}`;
const env = { ...process.env, OTTO_WORKSPACE: workspace, OTTO_API_KEY: 'docker-smoke-fixture' };
const compose = (...args) => execFileSync('docker', ['compose', '-p', project, ...args], {
  cwd: root, env, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'], timeout: 60_000,
});
const request = (path, token, options = {}) => fetch(`http://127.0.0.1:8787${path}`, {
  ...options,
  headers: token ? { Authorization: `Bearer ${token}`, ...options.headers } : options.headers,
  signal: AbortSignal.timeout(3000),
});
const pause = () => new Promise((resolve) => setTimeout(resolve, 300));
async function ready() {
  for (let attempt = 0; attempt < 100; attempt++) {
    const token = compose('logs', '--no-color', 'otto').match(/\?token=([^\s]+)/)?.[1];
    if (token) {
      try {
        if ((await request('/healthz')).ok) return token;
      } catch {}
    }
    await pause();
  }
  throw new Error('Container server did not become ready within 30 seconds');
}

try {
  assert.match(compose('run', '--rm', '--no-deps', 'otto', '--version'), /^otto /);
  assert.equal(compose('run', '--rm', '--no-deps', '--entrypoint', 'id', 'otto', '-u').trim(), '1000');
  // Use the real setup flow: configuration must survive container recreation.
  execFileSync('docker', ['compose', '-p', project, 'run', '--rm', '--no-deps', '-T', 'otto', 'setup'], {
    cwd: root, env, encoding: 'utf8', timeout: 60_000,
    input: 'openai-compatible\ndocker-smoke\ndocker-smoke-model\nhttp://127.0.0.1:9/v1\nOTTO_API_KEY\nsave\n',
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  compose('up', '-d', '--no-build', '--pull', 'never');
  const token = await ready();
  assert.equal((await request('/v1/sessions')).status, 401);
  assert.equal((await request('/v1/sessions', 'invalid-token')).status, 401);
  assert.equal((await request('/v1/sessions', token)).status, 200);
  assert.equal((await request('/')).status, 404);
  assert.equal((await request('/assets/app.js')).status, 404);
  const created = await request('/v1/sessions', token, { method: 'POST', body: '{}' });
  assert.equal(created.status, 201);
  const { id } = await created.json();
  assert.ok(id, 'A session must be created without calling a provider');
  // Empty sessions are lazy; renaming publishes a durable session_info entry.
  const renamed = await request(`/v1/sessions/${id}`, token, {
    method: 'PATCH', body: JSON.stringify({ name: 'docker-smoke-session' }),
  });
  assert.equal(renamed.status, 200);
  compose('exec', '-T', 'otto', 'sh', '-c', 'printf docker-smoke > /workspace/marker');
  assert.equal(readFileSync(join(workspace, 'marker'), 'utf8'), 'docker-smoke');
  const container = compose('ps', '-q', 'otto').trim();
  compose('stop');
  const code = execFileSync('docker', ['inspect', '--format', '{{.State.ExitCode}}', container], {
    encoding: 'utf8', timeout: 10_000,
  }).trim();
  assert.equal(code, '0', 'Otto must shut down cleanly on SIGTERM');
  compose('down'); // Preserve volume, but replace both network namespace users.
  compose('up', '-d', '--no-build', '--pull', 'never');
  const nextToken = await ready();
  assert.notEqual(nextToken, token, 'Access token must rotate after restart');
  assert.equal((await request('/v1/sessions', token)).status, 401);
  const listing = await request('/v1/sessions', nextToken);
  const persisted = (await listing.json()).sessions.find((session) => session.id === id);
  assert.equal(persisted?.name, 'docker-smoke-session', 'Session must survive recreation');
  assert.equal(readFileSync(join(workspace, 'marker'), 'utf8'), 'docker-smoke');
  console.log('Docker smoke passed: non-root CLI, setup persistence, API-only routes, token gating, sessions, workspace writes and shutdown.');
} finally {
  try { compose('down', '-v', '--remove-orphans'); }
  finally { rmSync(workspace, { recursive: true, force: true }); }
}
