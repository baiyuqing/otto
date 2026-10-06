#!/usr/bin/env node
// Local gate behind `make check-fast`: runs only the checks that cover the
// paths changed since the merge base with origin/main (committed, staged,
// unstaged and untracked). `make check-fast` holds a lock shared by all
// worktrees, so one runs at a time. See "Local verification" in docs/development.md.
import { execFileSync, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT_RUST = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml'];
const FULL_FILES = [...ROOT_RUST, 'crates/otto/Cargo.toml', 'crates/otto/build.rs', 'crates/otto/src/lib.rs', 'crates/otto/src/main.rs'];
const LICENSE_INPUTS = /^(Cargo\.toml|Cargo\.lock|crates\/[^/]+\/Cargo\.toml|scripts\/license-check\.mjs)$/;

// plan maps repo-relative POSIX paths to the ordered commands that check them
// and the sorted list of paths no rule covers.
export function plan(paths) {
  const has = (f) => paths.some(f);
  const unchecked = [];
  const modules = new Set();
  const tests = new Set();
  let full = false;
  for (const p of paths) {
    const inRust = p.startsWith('crates/') || p.startsWith('testdata/');
    const known =
      inRust || ROOT_RUST.includes(p) || /^(connect|scripts|docs)\//.test(p) || p === 'LICENSE' || p.endsWith('.md');
    if (!known) unchecked.push(p);
    if (ROOT_RUST.includes(p) || p.startsWith('testdata/') || p.startsWith('crates/otto-core/') || FULL_FILES.includes(p)) full = true;
    else if (inRust) {
      const mod = p.match(/^crates\/otto\/src\/([^/.]+)(?:\/|\.rs$)/);
      const test = p.match(/^crates\/otto\/tests\/([^/]+)\.rs$/);
      if (mod) modules.add(mod[1]);
      else if (test) tests.add(test[1]);
      else full = true; // conservative: any other crates/ path
    }
  }
  const under = (d) => has((p) => p.startsWith(d));
  const rustPath = has((p) => p.startsWith('crates/') || p.startsWith('testdata/') || ROOT_RUST.includes(p));
  const steps = [];
  if (has((p) => p.endsWith('.rs'))) steps.push('cargo fmt --all -- --check');
  if (rustPath) steps.push('cargo clippy --workspace --all-targets -- -D warnings');
  if (has((p) => ROOT_RUST.includes(p)) || under('crates/otto-core/')) steps.push('make rust-wasm-check');
  if (full) steps.push('cargo test --workspace');
  else {
    // --workspace, not -p otto: -p resolves other features and rebuilds dependencies.
    if (modules.size) steps.push(`cargo test --workspace --lib -- ${[...modules].sort().map((m) => `${m}::`).join(' ')}`);
    if (tests.size) steps.push(`cargo test --workspace ${[...tests].sort().map((t) => `--test ${t}`).join(' ')}`);
  }
  if (under('chat-connector/')) steps.push('make connect-check');
  if (under('scripts/')) steps.push('make scripts-test');
  if (has((p) => LICENSE_INPUTS.test(p))) steps.push('make license-check');
  return { steps, unchecked: unchecked.sort() };
}

function main() {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();
  let base;
  for (const ref of ['origin/main', 'main']) {
    try {
      base = git('merge-base', 'HEAD', ref);
      break;
    } catch {}
  }
  if (!base) {
    console.error('check-fast: no merge base with origin/main or main');
    process.exit(1);
  }
  const short = base.slice(0, 9);
  const files = [...new Set([...git('diff', '--name-only', base).split('\n'), ...git('ls-files', '--others', '--exclude-standard').split('\n')].filter(Boolean))];
  if (!files.length) {
    console.log(`check-fast: no changes since ${short}`);
    return;
  }
  const { steps, unchecked } = plan(files);
  console.log(`check-fast: ${files.length} changed paths since ${short}`);
  for (const s of steps) console.log(`  run: ${s}`);
  if (unchecked.length) console.log(`check-fast: not checked here (CI runs the full gate): ${unchecked.join(' ')}`);
  for (const s of [...steps, `git diff --check ${base}`]) {
    const r = spawnSync(s, { cwd: root, shell: true, stdio: 'inherit' });
    if (r.status !== 0) {
      console.error(`check-fast: failed: ${s}`);
      process.exit(r.status ?? 1);
    }
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
