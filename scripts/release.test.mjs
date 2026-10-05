import assert from 'node:assert/strict';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { FILES, bump, checkBump, readWorkspaceVersion, rewriteCargoToml, rewriteDesktopConfig, rewriteLock } from './release.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));

test('checkBump rejects malformed and non-increasing versions', () => {
  for (const [next, cur] of [['0.1.0', '0.1.0'], ['0.1.9', '0.2.0'], ['1.0', '0.1.0'], ['v1.0.0', '0.1.0'], ['01.0.0', '0.1.0'], ['1.0.0-rc1', '0.1.0']]) {
    assert.throws(() => checkBump(next, cur), Error, `${cur} -> ${next}`);
  }
});

test('checkBump accepts greater versions, comparing numerically', () => {
  for (const [next, cur] of [['0.1.1', '0.1.0'], ['0.2.0', '0.1.9'], ['0.10.0', '0.9.0'], ['1.0.0', '0.99.99']]) {
    assert.doesNotThrow(() => checkBump(next, cur), `${cur} -> ${next}`);
  }
});

test('Cargo.toml rewrite changes only [workspace.package] version', () => {
  const src = '[workspace]\nmembers = []\n\n[workspace.package]\nedition = "2024"\nversion = "0.1.0"\n\n[workspace.dependencies]\nfoo = { version = "0.1.0" }\nversion = "0.1.0"\n';
  assert.equal(readWorkspaceVersion(src), '0.1.0');
  assert.equal(rewriteCargoToml(src, '0.2.0'), src.replace('\nversion = "0.1.0"\n\n[workspace.dependencies]', '\nversion = "0.2.0"\n\n[workspace.dependencies]'));
  assert.throws(() => rewriteCargoToml('[package]\nversion = "0.1.0"\n', '0.2.0'));
});

test('MyGo configuration rewrite changes only the top-level version', () => {
  const src = '{\n  "name": "Otto",\n  "version": "0.1.0",\n  "macos": {\n    "version": "0.1.0"\n  }\n}\n';
  assert.equal(rewriteDesktopConfig(src, '0.2.0'), src.replace('  "version": "0.1.0",', '  "version": "0.2.0",'));
  assert.throws(() => rewriteDesktopConfig('{ "name": "x" }', '0.2.0'));
});

test('lockfile rewrite matches package names exactly', () => {
  const pkg = (n, v) => `[[package]]\nname = "${n}"\nversion = "${v}"\ndependencies = []\n\n`;
  const src = pkg('otto', '0.1.0') + pkg('otto-extra', '0.1.0') + pkg('otto-core', '0.1.0') + pkg('otto-web', '0.1.0') + pkg('serde', '0.1.0');
  const want = pkg('otto', '0.2.0') + pkg('otto-extra', '0.1.0') + pkg('otto-core', '0.2.0') + pkg('otto-web', '0.2.0') + pkg('serde', '0.1.0');
  assert.equal(rewriteLock(src, ['otto', 'otto-core', 'otto-web'], '0.2.0'), want);
  assert.throws(() => rewriteLock(src, ['otto-missing'], '0.2.0'), /otto-missing/);
  assert.throws(() => rewriteLock(src + pkg('otto', '0.1.0'), ['otto'], '0.2.0'), /found 2/);
});

test('bump rewrites a copy of the real files and keeps them consistent', () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'otto-release-'));
  try {
    mkdirSync(path.join(dir, 'desktop'), { recursive: true });
    for (const [file] of FILES) cpSync(path.join(root, file), path.join(dir, file));
    const current = readWorkspaceVersion(readFileSync(path.join(dir, 'Cargo.toml'), 'utf8'));
    assert.throws(() => bump(dir, current));
    bump(dir, '99.0.0');
    for (const [file] of FILES) {
      const before = readFileSync(path.join(root, file), 'utf8');
      const after = readFileSync(path.join(dir, file), 'utf8');
      assert.notEqual(after, before, file);
      const changedLines = after.split('\n').filter((l, i) => l !== before.split('\n')[i]);
      assert.ok(changedLines.every((l) => l.includes('99.0.0')), `${file}: only version lines change`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
