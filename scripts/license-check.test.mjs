import assert from 'node:assert/strict';
import test from 'node:test';
import { allowed, rustPackages, violations } from './license-check.mjs';

test('allowed evaluates SPDX expressions', () => {
  const yes = [
    'MIT',
    'MIT-0',
    'MIT OR GPL-3.0',
    'GPL-3.0 OR MIT AND Apache-2.0',
    '(MIT OR Apache-2.0) AND Unicode-3.0',
    'Apache-2.0 WITH LLVM-exception OR MIT',
    'MIT/Apache-2.0',
    'Apache-2.0 / MIT / MPL-2.0',
    'MIT or Apache-2.0',
  ];
  const no = [
    'GPL-3.0',
    'MIT AND GPL-3.0',
    'GPL-3.0 OR MIT AND LGPL-2.1',
    '(MIT OR GPL-2.0) AND MPL-2.0',
    'GPL-2.0 WITH Classpath-exception-2.0',
    'GPL-3.0/LGPL-3.0',
    'GPL-2.0+',
    'LGPL-2.1-or-later',
    'MPL-2.0',
    'mit',
  ];
  for (const e of yes) assert.equal(allowed(e), true, e);
  for (const e of no) assert.equal(allowed(e), false, e);
});

test('allowed rejects malformed and missing expressions without throwing', () => {
  for (const e of ['(MIT', 'MIT)', 'MIT OR', 'OR MIT', 'MIT AND AND MIT', 'MIT WITH', 'MIT MIT', '()', '', '  ', null, undefined, 42]) assert.equal(allowed(e), false, String(e));
});

const dep = (pkg, ...kinds) => ({ pkg, dep_kinds: kinds.map((kind) => ({ kind })) });
const meta = (graph) => ({
  workspace_members: ['W'],
  packages: Object.keys(graph).map((id) => ({ id, name: id.toLowerCase(), version: '1.0.0', license: id === 'B' ? undefined : 'MIT' })),
  resolve: { nodes: Object.entries(graph).map(([id, deps]) => ({ id, deps })) },
});

test('rustPackages follows normal and build edges, not dev-only edges', () => {
  const m = meta({
    W: [dep('A', null), dep('B', 'build'), dep('D', 'dev')],
    A: [dep('C', null)],
    B: [],
    C: [],
    D: [dep('E', null)],
    E: [],
  });
  assert.deepEqual(rustPackages(m).map((p) => p.name).sort(), ['a', 'b', 'c']);
  assert.equal(rustPackages(m).find((p) => p.name === 'b').license, null);
});

test('rustPackages keeps a crate that is a dev dep of one package and a normal dep of another', () => {
  const m = meta({ W: [dep('X', 'dev'), dep('A', null)], A: [dep('X', null)], X: [] });
  assert.deepEqual(rustPackages(m).map((p) => p.name).sort(), ['a', 'x']);
});

test('rustPackages counts an edge with both dev and normal kinds', () => {
  const m = meta({ W: [dep('X', 'dev', null)], X: [] });
  assert.deepEqual(rustPackages(m).map((p) => p.name), ['x']);
});

test('violations applies CLARIFIED by exact version and reports disallowed licenses', () => {
  const out = violations([
    { name: 'cfg_block', version: '0.1.1', license: null },
    { name: 'cfg_block', version: '0.1.2', license: null },
    { name: 'g', version: '1.0.0', license: 'GPL-3.0' },
    { name: 'ok', version: '1.0.0', license: 'MIT OR GPL-3.0' },
  ]);
  assert.deepEqual(out.map((p) => `${p.name}@${p.version}`), ['cfg_block@0.1.2', 'g@1.0.0']);
});
