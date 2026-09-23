import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { parseSession, windowPeaks, report } from "./skill-exec-measure.mjs";

/// One assistant turn that reports `input` context tokens and makes `calls`.
function assistant(id, input, calls = []) {
  return JSON.stringify({
    type: "message",
    id,
    message: {
      role: "assistant",
      content: calls.map((call, index) => ({
        type: "toolCall",
        id: `call-${id}-${index}`,
        name: call.tool,
        arguments: call.arguments,
      })),
      usage: { input, output: 1 },
    },
  });
}

const header = JSON.stringify({ type: "session", version: 3, id: "s-1" });

test("the session's peak is the largest single request, not the sum", () => {
  const text = [header, assistant("a", 100), assistant("b", 900), assistant("c", 300)].join("\n");

  const parsed = parseSession(text);

  assert.equal(parsed.sessionId, "s-1");
  assert.equal(parsed.peakInput, 900);
  assert.equal(parsed.totals.input, 1300);
});

test("routing counts an inline load and a delegation under the same name", () => {
  const text = [
    header,
    assistant("a", 10, [{ tool: "skill", arguments: { name: "normalize" } }]),
    assistant("b", 20, [{ tool: "agent", arguments: { agent: "normalize", prompt: "go" } }]),
    assistant("c", 30, [{ tool: "agent", arguments: { agent: "normalize", prompt: "again" } }]),
    assistant("d", 40, [{ tool: "read", arguments: { path: "x" } }]),
  ].join("\n");

  const parsed = parseSession(text);

  assert.deepEqual(parsed.routing.normalize, { inline: 1, delegated: 2 });
  assert.equal(parsed.routing.read, undefined, "only skill and agent calls are routing");
});

test("a skill file read does not count as an inline load", () => {
  const text = [
    header,
    assistant("a", 10, [{ tool: "skill", arguments: { name: "normalize", file: "scripts/run.py" } }]),
  ].join("\n");

  assert.deepEqual(parseSession(text).routing.normalize, { inline: 0, delegated: 0 });
});

test("a malformed line is skipped rather than failing the run", () => {
  const text = [header, "{not json", assistant("a", 42)].join("\n");

  assert.equal(parseSession(text).peakInput, 42);
});

test("window peaks separate the main context from each sub-agent", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "kite-measure-"));
  try {
    const database = path.join(directory, "usage.db");
    await seed(database, [
      { session: "s-1", task: "", input: 500 },
      { session: "s-1", task: "", input: 800 },
      { session: "s-1", task: "t-1", input: 200 },
      { session: "s-1", task: "t-1", input: 250 },
      { session: "s-2", task: "", input: 9000 },
    ]);

    const peaks = windowPeaks(database, "s-1");

    assert.deepEqual(peaks, [
      { window: "main", peakInput: 800, requests: 2 },
      { window: "t-1", peakInput: 250, requests: 2 },
    ]);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("the report names the largest window, which is what the design compares", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "kite-measure-"));
  try {
    const database = path.join(directory, "usage.db");
    await seed(database, [
      { session: "s-1", task: "", input: 800 },
      { session: "s-1", task: "t-1", input: 250 },
    ]);
    const text = [header, assistant("a", 800, [{ tool: "agent", arguments: { agent: "normalize" } }])].join("\n");

    const printed = report(parseSession(text), windowPeaks(database, "s-1"));

    assert.match(printed, /peak across all windows\s*:\s*800/);
    assert.match(printed, /normalize/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

/// Writes rows in the shape `crates/kite/src/usage.rs` creates.
async function seed(database, rows) {
  const { DatabaseSync } = await import("node:sqlite");
  const connection = new DatabaseSync(database);
  connection.exec(`CREATE TABLE usage_events (
    id INTEGER PRIMARY KEY, occurred_at TEXT NOT NULL, workspace TEXT NOT NULL,
    session_id TEXT NOT NULL, task_id TEXT NOT NULL, provider TEXT NOT NULL,
    profile TEXT NOT NULL, model TEXT NOT NULL, kind TEXT NOT NULL,
    input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
    cached_input_tokens INTEGER NOT NULL, usage_present INTEGER NOT NULL) STRICT;`);
  const insert = connection.prepare(
    `INSERT INTO usage_events (occurred_at, workspace, session_id, task_id, provider,
      profile, model, kind, input_tokens, output_tokens, cached_input_tokens, usage_present)
     VALUES (?, '/w', ?, ?, 'openai-compatible', 'p', 'm', 'provider', ?, 1, 0, 1)`,
  );
  for (const [index, row] of rows.entries()) {
    insert.run(`2026-09-22T0${index}:00:00.000000000Z`, row.session, row.task, row.input);
  }
  connection.close();
}
