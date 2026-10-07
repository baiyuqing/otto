import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const SCRIPT = fileURLToPath(new URL('./otto-tcp.sh', import.meta.url));

// A fake `otto` and a fake `otto-connect`, so these tests need neither a built
// binary, a real serve, nor a chat platform. The fakes record the argument
// vector they were started with: for connect, whether serve had already
// announced its ACP listener, which is the order the supervisor must keep.
const FAKE_OTTO = `#!/usr/bin/env bash
for arg in "$@"; do printf 'arg=%s\\n' "$arg"; done >> "\${FAKE_OTTO_ARGS:?}"
if [[ -f "\${FAKE_DIR:?}/serve-fail" ]]; then exit 3; fi
printf 'otto serve: ACP 127.0.0.1:9001\\n'
trap 'exit 0' TERM INT
while :; do sleep 0.05; done
`;

const FAKE_CONNECT = `#!/usr/bin/env bash
{
  if grep -q -F 'otto serve: ACP' "\${FAKE_SERVE_LOG:?}" 2>/dev/null; then
    printf 'serve_ready=yes\\n'
  else
    printf 'serve_ready=no\\n'
  fi
  for arg in "$@"; do printf 'arg=%s\\n' "$arg"; done
} >> "\${FAKE_CONNECT_ARGS:?}"
if [[ -f "\${FAKE_DIR:?}/connect-fail" ]]; then exit 4; fi
trap 'exit 0' TERM INT
while :; do sleep 0.05; done
`;

// capturedArgs returns the recorded argument vector, one line per argument, so
// an empty or reordered argument cannot hide in a joined string.
function capturedArgs(file) {
  return readFileSync(file, 'utf8')
    .split('\n')
    .filter((line) => line.startsWith('arg='))
    .map((line) => line.slice(4));
}

function writeExecutable(path, text) {
  writeFileSync(path, text);
  chmodSync(path, 0o755);
}

function context(t) {
  const dir = mkdtempSync(join(tmpdir(), 'otto-tcp-'));
  const state = join(dir, 'state');
  const bin = join(dir, 'bin');
  const workspace = join(dir, 'workspace');
  mkdirSync(bin);
  mkdirSync(workspace);
  writeExecutable(join(bin, 'otto'), FAKE_OTTO);
  writeExecutable(join(bin, 'otto-connect'), FAKE_CONNECT);

  const paths = {
    dir,
    state,
    bin,
    workspace,
    ottoArgs: join(dir, 'otto.args'),
    connectArgs: join(dir, 'connect.args'),
    envFile: join(dir, 'otto.env'),
    config: join(dir, 'connect.toml'),
    serveLog: join(state, 'serve.log'),
    supervisorLog: join(state, 'supervisor.log'),
    supervisorPid: join(state, 'supervisor.pid'),
    servePid: join(state, 'serve.pid'),
    connectPid: join(state, 'connect.pid'),
  };

  writeFileSync(
    paths.envFile,
    'OTTO_PROVIDER=openai-compatible\nOTTO_MODEL=example-model\n',
  );
  writeConnectConfig(paths, {});

  const env = {
    PATH: process.env.PATH,
    HOME: dir,
    OTTO_TCP_HOME: state,
    OTTO_ENV_FILE: paths.envFile,
    OTTO_BIN: join(bin, 'otto'),
    OTTO_CONNECT_BIN: join(bin, 'otto-connect'),
    OTTO_CONNECT_CONFIG: paths.config,
    OTTO_ACP_TOKEN: 'test-token-123',
    // Fast ticks and no backoff keep the restart tests quick; the defaults are
    // the documented ones.
    OTTO_TCP_POLL: '0.05',
    OTTO_TCP_MIN_DELAY: '0',
    OTTO_TCP_READY_TIMEOUT: '5',
    OTTO_TCP_STOP_TIMEOUT: '5',
    FAKE_DIR: dir,
    FAKE_OTTO_ARGS: paths.ottoArgs,
    FAKE_CONNECT_ARGS: paths.connectArgs,
    FAKE_SERVE_LOG: paths.serveLog,
  };

  const run = (args = [], extra = {}) =>
    spawnSync('bash', [SCRIPT, ...args], { encoding: 'utf8', env: { ...env, ...extra } });

  // The supervisor removes a pid file when its child exits and writes it again
  // after the restart, so the file can disappear between any check and the read.
  const readPid = (file) => {
    let text;
    try {
      text = readFileSync(file, 'utf8').trim();
    } catch (error) {
      if (error.code === 'ENOENT') return null;
      throw error;
    }
    return /^[0-9]+$/.test(text) ? Number(text) : null;
  };

  const processAlive = (pid) => {
    if (pid === null) return false;
    try {
      process.kill(pid, 0);
      return true;
    } catch {
      return false;
    }
  };

  const waitFor = async (check, message, timeoutMs = 8000) => {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const value = await check();
      if (value) return value;
      if (Date.now() > deadline) throw new Error(`timed out waiting for ${message}`);
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  };

  // A stopped stack first: a failed test must not leave a supervisor behind.
  t.after(() => {
    run(['stop']);
    rmSync(dir, { recursive: true, force: true });
  });

  return { ...paths, env, run, readPid, processAlive, waitFor };
}

// writeConnectConfig writes a connect.toml, overridable key by key.
function writeConnectConfig(paths, overrides) {
  const agent = {
    address: '"127.0.0.1:9001"',
    token_env: '"OTTO_ACP_TOKEN"',
    workspace: `"${paths.workspace}"`,
    ...overrides.agent,
  };
  const lines = ['[agent]'];
  for (const [key, value] of Object.entries(agent)) {
    if (value !== null) lines.push(`${key} = ${value}`);
  }
  lines.push('', '[telegram]', 'token_env = "OTTO_CONNECT_TELEGRAM_TOKEN"', 'chats = ["1"]', 'senders = ["1"]', '');
  writeFileSync(paths.config, lines.join('\n'));
}

const start = (paths, extra) => paths.run(['start'], extra);
const statusLine = (output, label) =>
  output.split('\n').find((line) => line.startsWith(label)) ?? '';

test('starts serve with the otto.env settings and connect against the ACP port', async (t) => {
  const paths = context(t);
  const result = start(paths);
  assert.equal(result.status, 0, result.stderr);

  assert.match(statusLine(result.stdout, 'supervisor:'), /running \(pid \d+/);
  assert.match(statusLine(result.stdout, 'otto serve:'), /running \(pid \d+, up .*, ACP 127\.0\.0\.1:9001\)/);
  assert.match(statusLine(result.stdout, 'connect:'), /running \(pid \d+/);
  assert.match(statusLine(result.stdout, 'state:'), new RegExp(paths.state.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));

  assert.deepEqual(capturedArgs(paths.ottoArgs), [
    'serve',
    '--provider', 'openai-compatible',
    '--model', 'example-model',
    '--cwd', paths.workspace,
    '--acp-listen', '127.0.0.1:9001',
  ]);
  // The fake records its argv as soon as it starts, which can be a moment
  // after start returns, so wait for the record rather than assuming it.
  await paths.waitFor(() => {
    if (!existsSync(paths.connectArgs)) return false;
    return capturedArgs(paths.connectArgs).length > 0;
  }, 'connect to record its arguments');
  assert.match(readFileSync(paths.connectArgs, 'utf8'), /serve_ready=yes/);
  assert.deepEqual(capturedArgs(paths.connectArgs), ['--config', paths.config]);

  const servePid = paths.readPid(paths.servePid);
  const connectPid = paths.readPid(paths.connectPid);
  assert.ok(paths.processAlive(servePid), 'serve should be running');
  assert.ok(paths.processAlive(connectPid), 'connect should be running');
  // The serve log carries the serve HTTP token URL and the connect log chat
  // ids, so state and logs are private to the user.
  assert.equal(statSync(paths.state).mode & 0o777, 0o700);
  assert.equal(statSync(paths.serveLog).mode & 0o777, 0o600);
  assert.equal(statSync(paths.connectPid).mode & 0o777, 0o600);
});

test('stop stops both processes, clears the state and reports it afterwards', async (t) => {
  const paths = context(t);
  assert.equal(start(paths).status, 0);
  const servePid = paths.readPid(paths.servePid);
  const connectPid = paths.readPid(paths.connectPid);
  const supervisorPid = paths.readPid(paths.supervisorPid);

  const stopped = paths.run(['stop']);
  assert.equal(stopped.status, 0, stopped.stderr);
  for (const pid of [servePid, connectPid, supervisorPid]) {
    assert.equal(paths.processAlive(pid), false, `pid ${pid} should be gone`);
  }
  for (const file of ['supervisorPid', 'servePid', 'connectPid']) {
    assert.equal(existsSync(paths[file]), false, `${paths[file]} should be removed`);
  }

  const status = paths.run(['status']);
  assert.match(status.stdout, /supervisor:  not running/);
  assert.match(status.stdout, /otto serve:  not running/);
  assert.match(status.stdout, /connect:     not running/);

  const again = paths.run(['stop']);
  assert.equal(again.status, 0, again.stderr);
  assert.match(again.stdout, /not running/);
});

test('start refuses a second stack and restart replaces it', async (t) => {
  const paths = context(t);
  assert.equal(start(paths).status, 0);
  const firstSupervisor = paths.readPid(paths.supervisorPid);
  const second = start(paths);
  assert.equal(second.status, 1);
  assert.match(second.stderr, /already running/);

  const restarted = paths.run(['restart']);
  assert.equal(restarted.status, 0, restarted.stderr);
  const nextSupervisor = paths.readPid(paths.supervisorPid);
  assert.notEqual(nextSupervisor, firstSupervisor);
  await paths.waitFor(() => paths.processAlive(firstSupervisor) === false, 'the first supervisor to exit');
  assert.equal(paths.processAlive(nextSupervisor), true);
});

test('restarts a process that dies and leaves the other one running', async (t) => {
  const paths = context(t);
  assert.equal(start(paths).status, 0);
  const servePid = paths.readPid(paths.servePid);
  const connectPid = paths.readPid(paths.connectPid);

  process.kill(servePid, 'SIGTERM');
  const next = await paths.waitFor(() => {
    const pid = paths.readPid(paths.servePid);
    return pid !== null && pid !== servePid && paths.processAlive(pid) ? pid : null;
  }, 'serve to be restarted');
  assert.equal(paths.readPid(paths.connectPid), connectPid, 'connect should keep running');

  const log = readFileSync(paths.supervisorLog, 'utf8');
  assert.match(log, new RegExp(`serve exited pid=${servePid}`));
  assert.match(log, /serve restarting in 0s \(fast failure 1 of 5\)/);
  assert.equal(paths.processAlive(next), true);
});

test('restart serve and restart connect replace exactly one process', async (t) => {
  const paths = context(t);
  assert.equal(start(paths).status, 0);
  const servePid = paths.readPid(paths.servePid);
  const connectPid = paths.readPid(paths.connectPid);

  assert.equal(paths.run(['restart', 'serve']).status, 0);
  const nextServe = await paths.waitFor(() => {
    const pid = paths.readPid(paths.servePid);
    return pid !== null && pid !== servePid && paths.processAlive(pid) ? pid : null;
  }, 'serve to be replaced');
  assert.equal(paths.readPid(paths.connectPid), connectPid);

  assert.equal(paths.run(['restart', 'connect']).status, 0);
  const nextConnect = await paths.waitFor(() => {
    const pid = paths.readPid(paths.connectPid);
    return pid !== null && pid !== connectPid && paths.processAlive(pid) ? pid : null;
  }, 'connect to be replaced');
  assert.equal(paths.readPid(paths.servePid), nextServe, 'serve should keep running');
});

test('stops the stack after repeated fast failures instead of looping', async (t) => {
  const paths = context(t);
  writeFileSync(join(paths.dir, 'serve-fail'), '');
  const result = start(paths, { OTTO_TCP_MAX_FAILS: '2' });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /serve failed 2 times in a row; stopping the stack/);
  assert.match(readFileSync(paths.supervisorLog, 'utf8'), /supervisor stopped/);
  assert.match(paths.run(['status']).stdout, /supervisor:  not running/);
});

test('fails closed when connect.toml would start its own otto acp', (t) => {
  const paths = context(t);
  writeConnectConfig(paths, { agent: { command: '["otto", "acp"]', address: null } });
  const result = start(paths);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /connect\.toml sets \[agent\]\.command; connect would start its own otto acp/);
  assert.equal(existsSync(paths.ottoArgs), false, 'serve must not start');
  assert.equal(existsSync(paths.connectArgs), false, 'connect must not start');
});

test('fails closed when connect.toml names another ACP address or token', (t) => {
  const paths = context(t);
  writeConnectConfig(paths, { agent: { address: '"127.0.0.1:9999"' } });
  const address = start(paths);
  assert.equal(address.status, 1);
  assert.match(address.stderr, /\[agent\]\.address is '127\.0\.0\.1:9999' but this stack serves ACP on '127\.0\.0\.1:9001'/);

  writeConnectConfig(paths, { agent: { token_env: '"OTHER_TOKEN"' } });
  const token = start(paths);
  assert.equal(token.status, 1);
  assert.match(token.stderr, /token_env = "OTHER_TOKEN" but otto serve reads OTTO_ACP_TOKEN/);

  // The same value under another name is accepted, because connect then
  // authenticates with the token serve checks.
  const named = start(paths, { OTHER_TOKEN: 'test-token-123' });
  assert.equal(named.status, 0, named.stderr);
  paths.run(['stop']);
});

test('fails closed without a token, a config or a workspace', (t) => {
  const paths = context(t);
  const noToken = start(paths, { OTTO_ACP_TOKEN: '' });
  assert.equal(noToken.status, 1);
  assert.match(noToken.stderr, /OTTO_ACP_TOKEN is not set/);

  const noConfig = start(paths, { OTTO_CONNECT_CONFIG: join(paths.dir, 'absent.toml') });
  assert.equal(noConfig.status, 1);
  assert.match(noConfig.stderr, /connect configuration .*absent\.toml.* not found/);

  writeConnectConfig(paths, { agent: { workspace: '"relative/workspace"' } });
  const relative = start(paths);
  assert.equal(relative.status, 1);
  assert.match(relative.stderr, /\[agent\]\.workspace must be an absolute path/);

  // A workspace the reader cannot see at all is only reachable through the
  // documented escape hatch, and then OTTO_CWD has to name one.
  writeFileSync(paths.config, '[agent]\naddress = "127.0.0.1:9001"\n');
  writeFileSync(paths.envFile, 'OTTO_PROVIDER=openai-compatible\n');
  const noWorkspace = start(paths, { OTTO_TCP_SKIP_CHECKS: '1' });
  assert.equal(noWorkspace.status, 1);
  assert.match(noWorkspace.stderr, /no workspace: set OTTO_CWD in otto\.env/);
  assert.equal(existsSync(paths.ottoArgs), false, 'serve must not start');
});

test('warns instead of failing when OTTO_CWD is another workspace', (t) => {
  const paths = context(t);
  mkdirSync(join(paths.dir, 'other'));
  const result = start(paths, { OTTO_CWD: join(paths.dir, 'other') });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /differs from connect\.toml workspace/);
  const args = capturedArgs(paths.ottoArgs);
  assert.equal(args[args.indexOf('--cwd') + 1], join(paths.dir, 'other'));
  paths.run(['stop']);
});

test('skips the configuration checks when told to', (t) => {
  const paths = context(t);
  writeFileSync(paths.config, `[agent]\nworkspace = "${paths.workspace}"\n`);
  const result = start(paths, { OTTO_TCP_SKIP_CHECKS: '1', OTTO_CWD: paths.workspace });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /skipping the \[agent\] checks/);
  assert.match(readFileSync(paths.ottoArgs, 'utf8'), /--acp-listen/);
  assert.deepEqual(capturedArgs(paths.ottoArgs).slice(-2), ['--acp-listen', '127.0.0.1:9001']);
});

test('reports usage, unknown commands and a missing stack', (t) => {
  const paths = context(t);
  const help = paths.run(['help']);
  assert.equal(help.status, 0);
  assert.match(help.stdout, /otto-tcp\.sh start/);

  const unknown = paths.run(['frobnicate']);
  assert.equal(unknown.status, 2);
  assert.match(unknown.stderr, /unknown command frobnicate/);

  const extra = paths.run(['status', 'serve']);
  assert.equal(extra.status, 2);
  assert.match(extra.stderr, /status takes no arguments/);

  const restart = paths.run(['restart', 'serve']);
  assert.equal(restart.status, 1);
  assert.match(restart.stderr, /not running; use start/);
});

test('translates the remaining launcher settings into serve options', (t) => {
  const paths = context(t);
  writeFileSync(
    paths.envFile,
    [
      'OTTO_PROVIDER=chatgpt',
      'OTTO_MODEL=example-model',
      'OTTO_PROFILE=work',
      'OTTO_THINKING=high',
      'OTTO_SANDBOX=auto',
      'OTTO_SHELL_TIMEOUT=120s',
      'OTTO_MAX_OUTPUT_BYTES=65536',
      'OTTO_SERVE_EXTRA_ARGS="--no-mcp"',
      'OTTO_LISTEN=127.0.0.1:9100',
      'OTTO_UI=tui',
      '',
    ].join('\n'),
  );
  const result = start(paths);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /OTTO_UI and OTTO_EXTRA_ARGS are not used here/);
  assert.deepEqual(capturedArgs(paths.ottoArgs), [
    'serve',
    '--provider', 'chatgpt',
    '--model', 'example-model',
    '--profile', 'work',
    '--sandbox', 'auto',
    '--thinking', 'high',
    '--shell-timeout', '120s',
    '--max-output-bytes', '65536',
    '--no-mcp',
    '--cwd', paths.workspace,
    '--acp-listen', '127.0.0.1:9001',
    '--listen', '127.0.0.1:9100',
  ]);
  paths.run(['stop']);
});

test('ignores an empty OTTO_SERVE_EXTRA_ARGS instead of passing an empty argument', (t) => {
  const paths = context(t);
  const result = start(paths, { OTTO_SERVE_EXTRA_ARGS: '   ' });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(capturedArgs(paths.ottoArgs), [
    'serve',
    '--provider', 'openai-compatible',
    '--model', 'example-model',
    '--cwd', paths.workspace,
    '--acp-listen', '127.0.0.1:9001',
  ]);
  paths.run(['stop']);
});
