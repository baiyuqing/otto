#!/usr/bin/env node
// Release helper: `bump X.Y.Z` rewrites the version in every manifest and
// lockfile; `tag` tags origin/main and creates the GitHub release.
// See "Releases" in docs/development.md.
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

export function parseVersion(text) {
  const m = SEMVER.exec(text);
  if (!m) throw new Error(`invalid version "${text}": expected X.Y.Z without prefix or pre-release`);
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

export function isGreater(next, current) {
  const a = parseVersion(next);
  const b = parseVersion(current);
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] > b[i];
  return false;
}

export function checkBump(next, current) {
  parseVersion(next);
  if (!isGreater(next, current)) throw new Error(`version ${next} must be greater than the current version ${current}`);
}

// Replaces the single match of `re` (capture groups 1 and 2 surround the
// version) in `text`; any other match count is an error.
function replaceOnce(text, re, version, what) {
  const g = new RegExp(re.source, re.flags.includes('g') ? re.flags : re.flags + 'g');
  const n = [...text.matchAll(g)].length;
  if (n !== 1) throw new Error(`${what}: expected exactly 1 match, found ${n}`);
  return text.replace(re, (_, pre, post) => `${pre}${version}${post}`);
}

function inTomlSection(text, header, version, what) {
  const start = text.indexOf(`\n${header}\n`);
  if (start < 0 || text.indexOf(`\n${header}\n`, start + 1) >= 0) throw new Error(`${what}: section ${header} not found exactly once`);
  const bodyStart = start + header.length + 2;
  const next = text.indexOf('\n[', bodyStart);
  const end = next < 0 ? text.length : next;
  const body = replaceOnce(text.slice(bodyStart, end), /(^version = ")[^"]*(")/m, version, `${what} ${header} version`);
  return text.slice(0, bodyStart) + body + text.slice(end);
}

export const readWorkspaceVersion = (cargoToml) => {
  const section = /\n\[workspace\.package\]\n([\s\S]*?)(?:\n\[|$)/.exec('\n' + cargoToml);
  const m = section && /^version = "([^"]*)"/m.exec(section[1]);
  if (!m) throw new Error('Cargo.toml: [workspace.package] version not found');
  return m[1];
};

export const rewriteCargoToml = (t, v) => inTomlSection('\n' + t, '[workspace.package]', v, 'Cargo.toml').slice(1);
export function rewriteLock(text, names, v) {
  let out = text;
  for (const name of names) {
    out = replaceOnce(out, new RegExp(`(\\[\\[package\\]\\]\\nname = "${name}"\\nversion = ")[^"]*(")`), v, `lockfile package ${name}`);
  }
  return out;
}

export const FILES = [
  ['Cargo.toml', rewriteCargoToml],
  ['Cargo.lock', (t, v) => rewriteLock(t, ['otto', 'otto-core', 'otto-client', 'otto-terminal', 'otto-server', 'otto-acp', 'otto-eval', 'otto-cli'], v)],
];

export function bump(root, next) {
  const current = readWorkspaceVersion(readFileSync(path.join(root, 'Cargo.toml'), 'utf8'));
  checkBump(next, current);
  // Compute every result before writing so a failure leaves no partial change.
  const results = FILES.map(([file, fn]) => [file, fn(readFileSync(path.join(root, file), 'utf8'), next)]);
  for (const [file, content] of results) writeFileSync(path.join(root, file), content);
  return { current, files: results.map(([f]) => f) };
}

function git(root, ...args) {
  return execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();
}

function tag(root) {
  const version = readWorkspaceVersion(readFileSync(path.join(root, 'Cargo.toml'), 'utf8'));
  parseVersion(version);
  const name = `v${version}`;
  git(root, 'fetch', 'origin', 'main');
  if (git(root, 'rev-parse', 'HEAD') !== git(root, 'rev-parse', 'origin/main')) throw new Error('HEAD is not equal to origin/main; check out an up-to-date main');
  if (git(root, 'status', '--porcelain') !== '') throw new Error('working tree has uncommitted changes');
  if (git(root, 'tag', '--list', name) !== '') throw new Error(`tag ${name} already exists locally`);
  if (git(root, 'ls-remote', '--tags', 'origin', `refs/tags/${name}`) !== '') throw new Error(`tag ${name} already exists on origin`);
  git(root, 'tag', '-a', name, '-m', `otto ${name}`);
  git(root, 'push', 'origin', name);
  const release = ['release', 'create', name, '--generate-notes', '--verify-tag', '--title', name];
  try {
    execFileSync('gh', release, { cwd: root, stdio: 'inherit' });
  } catch (e) {
    // The tag is already on origin, so rerunning `tag` stops at the tag check.
    throw new Error(`tag ${name} was pushed but the release was not created (${e.message}); run: gh ${release.join(' ')}`);
  }
  console.log(`released ${name}`);
}

function main(argv) {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const [cmd, arg] = argv;
  if (cmd === 'bump' && arg) {
    const { current, files } = bump(root, arg);
    console.log(`version ${current} -> ${arg}; changed:\n${files.map((f) => `  ${f}`).join('\n')}`);
    console.log(`\nNext: commit on a branch, open a PR titled "Release v${arg}", and after it merges run \`make release-tag\` on an up-to-date main.`);
  } else if (cmd === 'tag' && !arg) {
    tag(root);
  } else {
    throw new Error('usage: release.mjs bump X.Y.Z | release.mjs tag');
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    main(process.argv.slice(2));
  } catch (e) {
    console.error(`error: ${e.message}`);
    process.exit(1);
  }
}
