import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { isWorkspaceArtifact, lockVersion, prune, workspaceNames } from './ci-cache.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));

const LOCK = `version = 4

[[package]]
name = "wasm-bindgen"
version = "0.2.128"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "wasm-bindgen-test"
version = "0.3.78"
`;

test('lockVersion reads the exact package, not one sharing its prefix', () => {
  assert.equal(lockVersion(LOCK, 'wasm-bindgen'), '0.2.128');
  assert.equal(lockVersion(LOCK, 'wasm-bindgen-test'), '0.3.78');
  assert.throws(() => lockVersion(LOCK, 'wasm'), /found 0/);
  assert.throws(() => lockVersion(LOCK + LOCK.slice(LOCK.indexOf('[[package]]')), 'wasm-bindgen'), /found 2/);
});

test('lockVersion finds the wasm-bindgen the workspace pins', () => {
  const lock = readFileSync(path.join(root, 'Cargo.lock'), 'utf8');
  const toml = readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  assert.match(toml, new RegExp(`^wasm-bindgen = "=${lockVersion(lock, 'wasm-bindgen').replaceAll('.', '\\.')}"$`, 'm'));
});

const names = workspaceNames({
  packages: [
    { name: 'otto', targets: [{ name: 'otto' }, { name: 'tui_pty' }, { name: 'build-script-build' }] },
    { name: 'otto-core', targets: [{ name: 'otto_core' }] },
  ],
});
const H = '0123456789abcdef';

test('isWorkspaceArtifact matches workspace output only', () => {
  const yes = [
    ['deps', `otto-${H}`], ['deps', `otto-${H}.d`], ['deps', `libotto-${H}.rlib`], ['deps', `libotto_core-${H}.rmeta`],
    ['deps', `tui_pty-${H}`], ['.fingerprint', `otto-core-${H}`], ['.fingerprint', `otto-${H}`], ['build', `otto-${H}`],
    ['incremental', `otto-${H}`], ['', 'otto'], ['', 'otto.d'], ['', 'libotto_core.rlib'],
  ];
  const no = [
    ['deps', `libserde-${H}.rlib`], ['deps', `tokio-${H}.d`], ['deps', `libotto_corex-${H}.rlib`], ['deps', 'otto-short'],
    ['.fingerprint', `serde-${H}`], ['.fingerprint', `otto-web-${H}`], ['build', `ring-${H}`], ['', '.cargo-lock'], ['', 'libserde.rlib'],
  ];
  for (const [dir, file] of yes) assert.ok(isWorkspaceArtifact(dir, file, names), `${dir}/${file}`);
  for (const [dir, file] of no) assert.ok(!isWorkspaceArtifact(dir, file, names), `${dir}/${file}`);
});

test('prune removes workspace output from every profile and keeps dependencies', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'ci-cache-'));
  try {
    const touch = (p) => {
      mkdirSync(path.dirname(path.join(dir, p)), { recursive: true });
      writeFileSync(path.join(dir, p), '');
    };
    const gone = [
      `debug/deps/otto-${H}`, `debug/deps/libotto-${H}.rlib`, `debug/.fingerprint/otto-${H}/lib-otto`,
      `debug/build/otto-${H}/output`, `debug/incremental/otto-${H}/s`, 'debug/otto',
      `wasm32-unknown-unknown/release/deps/libotto_core-${H}.rlib`, `wasm32-unknown-unknown/release/.fingerprint/otto-core-${H}/x`,
    ];
    const kept = [
      `debug/deps/libserde-${H}.rlib`, `debug/.fingerprint/serde-${H}/lib-serde`, `debug/build/ring-${H}/output`, 'debug/.cargo-lock',
      `wasm32-unknown-unknown/release/deps/libserde-${H}.rlib`,
    ];
    for (const p of [...gone, ...kept]) touch(p);
    prune(dir, names);
    for (const p of gone) assert.ok(!existsSync(path.join(dir, p)), `still there: ${p}`);
    for (const p of kept) assert.ok(existsSync(path.join(dir, p)), `removed: ${p}`);
    assert.ok(!existsSync(path.join(dir, 'debug/incremental')));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('the CLI rejects an unknown command', () => {
  assert.throws(() => execFileSync(process.execPath, [path.join(root, 'scripts/ci-cache.mjs'), 'nope'], { stdio: 'pipe' }), /status 2|usage/);
});
