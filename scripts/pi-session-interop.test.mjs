import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(new URL("./pi-session-interop.mjs", import.meta.url));

const fakePiModule = `
import { readFileSync } from "node:fs";

export const CURRENT_SESSION_VERSION = 3;
export const VERSION = "0.0.0-test";

function parseSession(sessionPath) {
  const lines = readFileSync(sessionPath, "utf8").split(/\\n/).filter(Boolean);
  const records = lines.map((line) => JSON.parse(line));
  const [header, ...entries] = records;
  return { header, entries };
}

function contextMessageForEntry(entry) {
  if (entry.type === "message") return entry.message;
  if (entry.type === "compaction") {
    return {
      role: "compactionSummary",
      summary: entry.summary,
      tokensBefore: entry.tokensBefore,
      timestamp: 0,
    };
  }
  return null;
}

export const SessionManager = {
  open(sessionPath) {
    const { header, entries } = parseSession(sessionPath);
    return {
      getHeader() {
        return header;
      },
      getEntries() {
        return entries;
      },
      buildSessionContext() {
        return {
          messages: entries.map(contextMessageForEntry).filter(Boolean),
          model: null,
          thinkingLevel: null,
        };
      },
      getSessionId() {
        return header.id;
      },
      getCwd() {
        return header.cwd;
      },
    };
  },
};
`;

async function withProbeFixture(lines, env, callback) {
  const tempDir = await mkdtemp(path.join(os.tmpdir(), "otto-pi-probe-"));
  try {
    const nodeModules = path.join(tempDir, "node_modules", "@earendil-works", "pi-coding-agent");
    await mkdir(nodeModules, { recursive: true });
    await writeFile(path.join(nodeModules, "package.json"), JSON.stringify({
      name: "@earendil-works/pi-coding-agent",
      version: "0.0.0-test",
      type: "module",
      main: "./index.mjs",
    }));
    await writeFile(path.join(nodeModules, "index.mjs"), fakePiModule);

    const sessionPath = path.join(tempDir, "session.jsonl");
    await writeFile(sessionPath, `${lines.join("\n")}\n`);

    const result = spawnSync(process.execPath, [scriptPath, sessionPath], {
      env: { ...process.env, NODE_PATH: path.join(tempDir, "node_modules"), ...env },
      encoding: "utf8",
    });
    await callback(result);
  } finally {
    await rm(tempDir, { recursive: true, force: true });
  }
}

test("counts Pi compactionSummary context metadata", async () => {
  await withProbeFixture([
    '{"type":"session","version":3,"id":"probe-first","timestamp":"2026-08-28T10:00:00Z","cwd":"/workspace"}',
    '{"type":"message","id":"a1","parentId":null,"timestamp":"2026-08-28T10:00:01Z","message":{"role":"user","content":"u1","timestamp":1}}',
    '{"type":"compaction","id":"a2","parentId":"a1","timestamp":"2026-08-28T10:00:02Z","summary":"summary","firstKeptEntryId":"a1","tokensBefore":17}',
    '{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-08-28T10:00:03Z","message":{"role":"assistant","content":"a1","api":"openai-completions","provider":"openai-compatible","model":"model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":3}}',
  ], { OTTO_PI_INTEROP: "1" }, async (result) => {
    assert.equal(result.status, 0, result.stderr);
    const output = JSON.parse(result.stdout);
    assert.equal(output.compaction.contextMessageCount, 1);
    assert.equal(output.compaction.contextMessagesWithTokensBefore, 1);
  });
});

test("accepts retainedTail empty array checkpoints", async () => {
  await withProbeFixture([
    '{"type":"session","version":3,"id":"probe-tail","timestamp":"2026-08-28T10:00:00Z","cwd":"/workspace"}',
    '{"type":"custom","id":"b1","parentId":null,"timestamp":"2026-08-28T10:00:01Z","customType":"otto.runtime","data":{"profile":"default","provider":"openai-compatible","model":"model"}}',
    '{"type":"compaction","id":"b2","parentId":"b1","timestamp":"2026-08-28T10:00:02Z","summary":"summary","tokensBefore":17,"retainedTail":[]}',
  ], { OTTO_PI_INTEROP: "1" }, async (result) => {
    assert.equal(result.status, 0, result.stderr);
    const output = JSON.parse(result.stdout);
    assert.equal(output.compaction.entriesWithRetainedTail, 1);
    assert.equal(output.compaction.retainedTailMessageCount, 0);
  });
});

test("requires OTTO_PI_INTEROP=1", async () => {
  await withProbeFixture([
    '{"type":"session","version":3,"id":"probe-gate","timestamp":"2026-08-28T10:00:00Z","cwd":"/workspace"}',
  ], {}, async (result) => {
    assert.equal(result.status, 77, result.stderr);
    assert.equal(result.stdout, "");
    assert.equal(result.stderr.trim(), "SKIP: OTTO_PI_INTEROP=1 is required to run the optional Pi interop probe");
  });
});
