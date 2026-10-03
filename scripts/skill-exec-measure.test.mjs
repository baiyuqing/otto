import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
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

test("window peaks asks Otto for the selected database and session", () => {
  const response = [
    { window: "main", peakInput: 800, requests: 2 },
    { window: "t-1", peakInput: 250, requests: 2 },
  ];
  let invocation;

  const peaks = windowPeaks("usage.db", "s-1", (...args) => {
    invocation = args;
    return JSON.stringify(response);
  });

  assert.deepEqual(peaks, response);
  assert.deepEqual(invocation, [
    process.env.OTTO_BIN || "otto",
    ["storage", "window-peaks", "usage.db", "s-1"],
    { encoding: "utf8" },
  ]);
});

test("subprocess errors propagate to the caller", () => {
  assert.throws(
    () => windowPeaks("usage.db", "s-1", () => { throw new Error("otto failed"); }),
    /otto failed/,
  );
});

test("the report names the largest window, which is what the design compares", () => {
  const text = [header, assistant("a", 800, [{ tool: "agent", arguments: { agent: "normalize" } }])].join("\n");
  const peaks = windowPeaks("usage.db", "s-1", () => JSON.stringify([
    { window: "main", peakInput: 800, requests: 2 },
    { window: "t-1", peakInput: 250, requests: 2 },
  ]));

  const printed = report(parseSession(text), peaks);

  assert.match(printed, /peak across all windows\s*:\s*800/);
  assert.match(printed, /normalize/);
});

test("the measurement script has no direct SQLite dependency", async () => {
  const source = await readFile(new URL("./skill-exec-measure.mjs", import.meta.url), "utf8");

  assert.doesNotMatch(source, /node:sqlite|DatabaseSync|CREATE TABLE usage_events/);
});
