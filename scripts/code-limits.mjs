#!/usr/bin/env node
// Code size and naming limits behind `make code-limits`, `make check-fast` and
// the pre-commit hook. A code file has at most MAX_LINES lines, a line at most
// MAX_LINE_CHARS characters (Unicode code points), and no file or Rust module
// is named after a generic container (GENERIC_NAMES). A Rust file under
// crates/*/src is compiled only under cfg(test) exactly when its path has a
// component named `tests` or `<domain>_tests`. Rules: AGENTS.md.
//
// `--staged` checks the staged content of staged code files (the hook); without
// it every tracked or untracked, not ignored, code file in the worktree.
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { basename, dirname, extname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const MAX_LINES = 800;
export const MAX_LINE_CHARS = 200;
export const CODE_EXTENSIONS = new Set(['.rs', '.go', '.js', '.mjs', '.py', '.sh']);
export const GENERIC_NAMES = new Set(['common', 'config', 'helper', 'helpers', 'misc', 'types', 'util', 'utils']);
// The configuration domain modules of otto-core and otto: the module is the
// domain itself and its files are named per configuration area.
export const GENERIC_NAME_EXCEPTIONS = new Set(['crates/otto-core/src/config/mod.rs', 'crates/otto/src/config/mod.rs']);

// moduleName is the name a search starts from: the file stem without Go's
// `_test` suffix; a Rust `mod.rs` takes its directory's name.
export function moduleName(path) {
  const stem = basename(path, extname(path));
  if (stem === 'mod' && extname(path) === '.rs') return basename(dirname(path));
  return stem.replace(/_test$/, '');
}

// violations returns one message per limit that the file at path with the
// given text breaks; files without a code extension are not checked.
export function violations(path, text) {
  if (!CODE_EXTENSIONS.has(extname(path))) return [];
  const found = [];
  const lines = text.split('\n');
  if (lines.at(-1) === '') lines.pop();
  if (lines.length > MAX_LINES) found.push(`${path}: ${lines.length} lines (limit ${MAX_LINES})`);
  lines.forEach((line, i) => {
    const chars = [...line].length;
    if (chars > MAX_LINE_CHARS) found.push(`${path}:${i + 1}: ${chars} characters (limit ${MAX_LINE_CHARS})`);
  });
  const name = moduleName(path);
  if (GENERIC_NAMES.has(name) && !GENERIC_NAME_EXCEPTIONS.has(path)) {
    found.push(`${path}: generic name "${name}"; name the file after its domain`);
  }
  return found;
}

export const CRATE_SOURCE = /^crates\/[^/]+\/src\/.+\.rs$/;
const CRATE_ROOT = /^crates\/[^/]+\/src\/(?:lib|main)\.rs$/;
const TEST_MODULE_NAME = /^(?:tests|\w+_tests)$/;
// An out-of-line `mod name;` at the start of a line with the attribute and
// comment lines directly above it.
const MOD_DECLARATION = /^((?:[ \t]*(?:#\[[^\]\n]*\][ \t]*|\/\/[^\n]*)\n)*)[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(\w+)\s*;/gm;

// hasTestModuleName reports whether a path below crates/*/src has a directory
// or file stem named `tests` or ending in `_tests`.
export function hasTestModuleName(path) {
  const components = path.replace(/^crates\/[^/]+\/src\//, '').replace(/\.rs$/, '').split('/');
  return components.some((component) => TEST_MODULE_NAME.test(component));
}

// cfgRequiresTest reports whether a cfg predicate, such as `all(test, unix)`,
// holds only when `test` is set.
export function cfgRequiresTest(predicate) {
  const tokens = predicate.match(/\w+|"[^"]*"|[(),=]/g) ?? [];
  let at = 0;
  const parse = () => {
    const name = tokens[at++];
    if (tokens[at] === '=') {
      at += 2;
      return false;
    }
    if (tokens[at] !== '(') return name === 'test';
    at++;
    const args = [];
    while (at < tokens.length && tokens[at] !== ')') {
      args.push(parse());
      if (tokens[at] === ',') at++;
    }
    at++;
    if (name === 'all') return args.some(Boolean);
    if (name === 'any') return args.length > 0 && args.every(Boolean);
    return false;
  };
  return parse();
}

// testOnlyFiles walks the module tree from each crate root in sources (path ->
// text) and returns the files declared only under a cfg that requires test,
// directly or through an ancestor module. `#[path]` is resolved from the
// declaring file's directory, and a `#[path]` file declares its children like
// a `mod.rs` file does.
export function testOnlyFiles(sources) {
  const reached = { test: new Set(), production: new Set() };
  const visit = (file, moduleDir, inTest) => {
    const seen = inTest ? reached.test : reached.production;
    if (seen.has(file)) return;
    seen.add(file);
    for (const [, attributes, name] of sources.get(file).matchAll(MOD_DECLARATION)) {
      const pathAttribute = attributes.match(/#\[path\s*=\s*"([^"]+)"\]/);
      const child = pathAttribute
        ? join(dirname(file), pathAttribute[1])
        : [`${moduleDir}/${name}.rs`, `${moduleDir}/${name}/mod.rs`].find((p) => sources.has(p));
      if (!sources.has(child)) continue;
      const childDir = pathAttribute || basename(child) === 'mod.rs' ? dirname(child) : child.slice(0, -'.rs'.length);
      const cfgTest = [...attributes.matchAll(/#\[cfg\((.*)\)\]/g)].some(([, predicate]) => cfgRequiresTest(predicate));
      visit(child, childDir, inTest || cfgTest);
    }
  };
  for (const root of sources.keys()) if (CRATE_ROOT.test(root)) visit(root, dirname(root), false);
  return new Set([...reached.test].filter((file) => !reached.production.has(file)));
}

// testModuleViolations returns one message per Rust file in sources whose
// test-module name and cfg(test) compilation disagree.
export function testModuleViolations(sources) {
  const testOnly = testOnlyFiles(sources);
  const found = [];
  for (const path of [...sources.keys()].sort()) {
    const named = hasTestModuleName(path);
    if (named && !testOnly.has(path)) {
      found.push(`${path}: named as a test module but not compiled only under cfg(test)`);
    } else if (!named && testOnly.has(path)) {
      found.push(`${path}: compiled only under cfg(test); name the file or a parent directory \`tests\` or \`<domain>_tests\``);
    }
  }
  return found;
}

// indexTexts returns the staged text of each path, read in one `git cat-file`.
function indexTexts(root, paths) {
  const input = paths.map((p) => `:${p}\n`).join('');
  const out = execFileSync('git', ['cat-file', '--batch'], { cwd: root, input, maxBuffer: 1 << 30 });
  const texts = new Map();
  let at = 0;
  for (const p of paths) {
    const headerEnd = out.indexOf(10, at);
    const size = Number(out.toString('latin1', at, headerEnd).split(' ')[2]);
    texts.set(p, out.toString('utf8', headerEnd + 1, headerEnd + 1 + size));
    at = headerEnd + 1 + size + 1;
  }
  return texts;
}

function main() {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8', maxBuffer: 1 << 30 });
  const staged = process.argv.includes('--staged');
  const listed = staged
    ? git('diff', '--cached', '--name-only', '--diff-filter=ACMR', '-z')
    : git('ls-files', '-z', '--cached', '--others', '--exclude-standard', '--deduplicate');
  const paths = listed.split('\0').filter((p) => CODE_EXTENSIONS.has(extname(p)));
  const found = [];
  const worktreeSources = new Map();
  for (const p of paths) {
    if (staged) found.push(...violations(p, git('show', `:${p}`)));
    else if (existsSync(join(root, p))) {
      const text = readFileSync(join(root, p), 'utf8');
      found.push(...violations(p, text));
      if (CRATE_SOURCE.test(p)) worktreeSources.set(p, text);
    }
  }
  // Module declarations span files, so the staged check reads the whole index.
  const sources = staged ? indexTexts(root, git('ls-files', '-z').split('\0').filter((p) => CRATE_SOURCE.test(p))) : worktreeSources;
  found.push(...testModuleViolations(sources));
  if (found.length) {
    console.error(found.join('\n'));
    console.error(`code-limits: ${found.length} violations; see "Code size and naming" in AGENTS.md`);
    process.exit(1);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
