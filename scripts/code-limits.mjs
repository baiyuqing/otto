#!/usr/bin/env node
// Code size and naming limits behind `make code-limits`, `make check-fast` and
// the pre-commit hook. A code file has at most MAX_LINES lines, a line at most
// MAX_LINE_CHARS characters (Unicode code points), and no file or Rust module
// is named after a generic container (GENERIC_NAMES). Rules: AGENTS.md.
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

function main() {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8', maxBuffer: 1 << 30 });
  const staged = process.argv.includes('--staged');
  const listed = staged
    ? git('diff', '--cached', '--name-only', '--diff-filter=ACMR', '-z')
    : git('ls-files', '-z', '--cached', '--others', '--exclude-standard', '--deduplicate');
  const paths = listed.split('\0').filter((p) => CODE_EXTENSIONS.has(extname(p)));
  const found = [];
  for (const p of paths) {
    if (staged) found.push(...violations(p, git('show', `:${p}`)));
    else if (existsSync(join(root, p))) found.push(...violations(p, readFileSync(join(root, p), 'utf8')));
  }
  if (found.length) {
    console.error(found.join('\n'));
    console.error(`code-limits: ${found.length} violations; see "Code size and naming" in AGENTS.md`);
    process.exit(1);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
