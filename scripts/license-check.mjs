#!/usr/bin/env node
// License allowlist check behind `make license-check` (part of `make check-linux`).
//
// Checked: every Rust crate in the non-dev dependency graph of the workspace
// (normal and build dependencies, all targets) from `cargo metadata`. Dev-only dependencies are
// skipped because they do not ship.
//
// The check reads the declared license fields only (Cargo `license`, npm
// `license` in the lock file). It does not inspect license files or source.
// A package without a license field fails unless CLARIFIED names it.
//
// ponytail: Go modules under connect/ are not checked because go.mod carries
// no license metadata (7 modules on 2026-10-05); go-licenses would be the
// upgrade path.
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export const ALLOWED = new Set(['MIT', 'MIT-0', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', 'ISC', 'Zlib', 'Unicode-3.0', 'Unlicense']);

// Packages without a license field, pinned by version so that a new version
// is reviewed again.
export const CLARIFIED = {
  // Declares only `license-file = "LICENSE"`; that file is the Apache-2.0 text.
  'cfg_block@0.1.1': 'Apache-2.0',
};

// allowed evaluates an SPDX license expression: every identifier must be in
// ALLOWED for the term to hold; AND binds tighter than OR; `X WITH e` is X.
// Cargo's legacy `A/B` means OR. Malformed or empty input is not allowed.
export function allowed(expr) {
  if (typeof expr !== 'string') return false;
  const tokens = expr.replaceAll('/', ' OR ').match(/\(|\)|[^\s()]+/g) ?? [];
  let i = 0;
  const op = (name) => tokens[i]?.toUpperCase() === name;
  const fail = () => {
    throw new SyntaxError(expr);
  };
  const term = () => {
    if (tokens[i] === '(') {
      i++;
      const v = or();
      if (tokens[i++] !== ')') fail();
      return v;
    }
    const id = tokens[i++];
    if (id === undefined || id === ')' || ['AND', 'OR', 'WITH'].includes(id.toUpperCase())) fail();
    if (op('WITH')) {
      i++;
      const exc = tokens[i++];
      if (exc === undefined || exc === '(' || exc === ')' || ['AND', 'OR', 'WITH'].includes(exc.toUpperCase())) fail();
    }
    return ALLOWED.has(id);
  };
  const and = () => {
    let v = term();
    while (op('AND')) {
      i++;
      v = term() && v;
    }
    return v;
  };
  const or = () => {
    let v = and();
    while (op('OR')) {
      i++;
      v = and() || v;
    }
    return v;
  };
  try {
    const v = or();
    return i === tokens.length && v;
  } catch (e) {
    if (e instanceof SyntaxError) return false;
    throw e;
  }
}

// rustPackages lists the non-workspace packages reachable from the workspace
// members through normal or build dependency edges.
export function rustPackages(metadata) {
  const byId = new Map(metadata.packages.map((p) => [p.id, p]));
  const nodes = new Map(metadata.resolve.nodes.map((n) => [n.id, n]));
  const members = new Set(metadata.workspace_members);
  const seen = new Set();
  const stack = [...members];
  while (stack.length) {
    for (const dep of nodes.get(stack.pop())?.deps ?? []) {
      if (seen.has(dep.pkg) || !dep.dep_kinds.some((k) => k.kind !== 'dev')) continue;
      seen.add(dep.pkg);
      stack.push(dep.pkg);
    }
  }
  return [...seen].filter((id) => !members.has(id)).map((id) => {
    const p = byId.get(id);
    return { name: p.name, version: p.version, license: p.license ?? null };
  });
}

// violations returns the packages whose license, after CLARIFIED, is not allowed.
export function violations(packages) {
  return packages.filter((p) => !allowed(CLARIFIED[`${p.name}@${p.version}`] ?? p.license));
}

function main() {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--locked'], { cwd: root, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 }));
  const rust = rustPackages(metadata);
  const bad = [
    ...violations(rust).map((p) => ['rust', p]),
  ];
  if (bad.length) {
    for (const [kind, p] of bad) console.error(`license-check: ${kind} ${p.name}@${p.version}: ${p.license ?? 'no license field'}`);
    console.error('license-check: review the license, then add its identifier to ALLOWED, or add a package without a license field to CLARIFIED, in scripts/license-check.mjs');
    process.exit(1);
  }
  console.log(`license-check: ${rust.length} Rust crates use allowed licenses`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
