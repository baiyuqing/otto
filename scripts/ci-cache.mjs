#!/usr/bin/env node
// CI cache helper for .github/workflows/checks.yml.
//
//   lock-version <crate>  print the version Cargo.lock pins for <crate>
//   prune [target-dir]    remove this workspace's own build output from
//                         target/ (default ./target) before it is cached
//
// The cargo cache holds third-party build output only: the workspace crates
// change on every commit, so caching their artifacts (the otto test binaries
// alone are several gigabytes) only makes the restore slower. See "CI" in
// docs/development.md.
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// Returns the version of the single Cargo.lock entry named `name`.
export function lockVersion(lockText, name) {
  const re = new RegExp(`^\\[\\[package\\]\\]\\nname = "${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"\\nversion = "([^"]+)"`, 'gm');
  const found = [...lockText.matchAll(re)].map((m) => m[1]);
  if (found.length !== 1) throw new Error(`Cargo.lock: expected exactly 1 package named ${name}, found ${found.length}`);
  return found[0];
}

// Names cargo uses for the workspace's artifacts, from `cargo metadata
// --no-deps`: package names name the .fingerprint and build directories,
// target names (with - as _) name the files in deps/ and the uplifted ones.
export function workspaceNames(metadata) {
  const packages = new Set();
  const targets = new Set();
  for (const pkg of metadata.packages) {
    packages.add(pkg.name);
    for (const t of pkg.targets) targets.add(t.name.replaceAll('-', '_'));
  }
  return { packages, targets };
}

const HASH = /^[0-9a-f]{16}$/;

// Splits `<name>-<16 hex>[.ext]` into its name, or returns null.
function hashedName(file) {
  const base = file.replace(/\..*$/, '');
  const i = base.lastIndexOf('-');
  if (i <= 0 || !HASH.test(base.slice(i + 1))) return null;
  return base.slice(0, i);
}

// Whether `file` in `dir` (one of a profile directory's subdirectories, or
// the profile directory itself) is output of a workspace crate.
export function isWorkspaceArtifact(dir, file, { packages, targets }) {
  if (dir === 'incremental') return true;
  if (dir === '.fingerprint' || dir === 'build') return packages.has(hashedName(file));
  if (dir === 'deps') {
    const name = hashedName(file);
    return name !== null && (targets.has(name) || (name.startsWith('lib') && targets.has(name.slice(3))));
  }
  // Uplifted files: target/debug/otto, otto.d, libotto_core.rlib, ...
  const stem = file.replace(/\..*$/, '');
  return targets.has(stem) || (stem.startsWith('lib') && targets.has(stem.slice(3)));
}

// Profile directories under target/: target/<profile> and
// target/<triple>/<profile>, recognised by their .fingerprint directory.
export function profileDirs(targetDir) {
  const out = [];
  const visit = (dir, depth) => {
    if (existsSync(path.join(dir, '.fingerprint'))) {
      out.push(dir);
      return;
    }
    if (depth === 0) return;
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (entry.isDirectory()) visit(path.join(dir, entry.name), depth - 1);
    }
  };
  if (existsSync(targetDir)) visit(targetDir, 2);
  return out;
}

// Removes the workspace's artifacts from every profile directory and returns
// the removed paths.
export function prune(targetDir, names) {
  const removed = [];
  const remove = (p) => {
    rmSync(p, { recursive: true, force: true });
    removed.push(p);
  };
  for (const profile of profileDirs(targetDir)) {
    for (const file of readdirSync(profile)) {
      const p = path.join(profile, file);
      if (file === 'incremental') remove(p);
      else if (!statSync(p).isDirectory() && isWorkspaceArtifact('', file, names)) remove(p);
    }
    for (const dir of ['deps', '.fingerprint', 'build']) {
      const sub = path.join(profile, dir);
      if (!existsSync(sub)) continue;
      for (const file of readdirSync(sub)) {
        if (isWorkspaceArtifact(dir, file, names)) remove(path.join(sub, file));
      }
    }
  }
  return removed;
}

function main([cmd, arg]) {
  const root = fileURLToPath(new URL('..', import.meta.url));
  if (cmd === 'lock-version' && arg) {
    console.log(lockVersion(readFileSync(path.join(root, 'Cargo.lock'), 'utf8'), arg));
  } else if (cmd === 'prune') {
    const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps', '--offline'], { cwd: root, encoding: 'utf8' }));
    const removed = prune(arg ?? path.join(root, 'target'), workspaceNames(metadata));
    console.log(`removed ${removed.length} workspace artifacts`);
  } else {
    console.error('usage: ci-cache.mjs lock-version <crate> | prune [target-dir]');
    process.exit(2);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main(process.argv.slice(2));
