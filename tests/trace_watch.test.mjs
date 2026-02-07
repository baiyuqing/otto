import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseDiff,
  summarizeNodes,
  sha256,
  getLatestConversation,
  getConversationWindow,
  getGitInfo,
  parseAst,
} from "../scripts/trace_watch.ts";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

test("parseDiff counts added and deleted lines", () => {
  const before = "a\nb\nc";
  const after = "a\nB\nc\nd";
  const diff = parseDiff(before, after);
  assert.equal(diff.added, 2);
  assert.equal(diff.deleted, 1);
  assert.ok(diff.hunks.length >= 1);
});

test("summarizeNodes formats summary", () => {
  const summary = summarizeNodes([
    { type: "function", name: "foo", start: 1, end: 3 },
    { type: "class", name: "Bar", start: 5, end: 9 },
  ]);
  assert.ok(summary.includes("function foo"));
  assert.ok(summary.includes("class Bar"));
});

test("sha256 produces deterministic hash", () => {
  assert.equal(sha256("abc"), sha256("abc"));
});

test("getLatestConversation reads JSONL last line", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-trace-"));
  const log = path.join(dir, "conversation.jsonl");
  fs.writeFileSync(
    log,
    JSON.stringify({ conversation_id: "c1", message_id: "m1", role: "user", content: "hello" }) + "\n" +
      JSON.stringify({ conversation_id: "c2", message_id: "m2", role: "assistant", content: "world" }) + "\n",
    "utf8",
  );
  const conv = getLatestConversation(log);
  assert.equal(conv.id, "c2");
  assert.equal(conv.message_id, "m2");
  assert.equal(conv.role, "assistant");
});

test("getConversationWindow returns recent messages in order", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-trace-"));
  const log = path.join(dir, "conversation.jsonl");
  fs.writeFileSync(
    log,
    JSON.stringify({ id: "c1", message_id: "m1", role: "user", created_at: "t1" }) + "\n" +
      JSON.stringify({ id: "c2", message_id: "m2", role: "assistant", created_at: "t2" }) + "\n" +
      JSON.stringify({ id: "c3", message_id: "m3", role: "assistant", created_at: "t3" }) + "\n",
    "utf8",
  );
  const window = getConversationWindow(log, 2);
  assert.equal(window.length, 2);
  assert.equal(window[0].message_id, "m2");
  assert.equal(window[1].message_id, "m3");
});

test("getGitInfo returns nulls outside a repo", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-trace-"));
  const info = getGitInfo(dir);
  assert.equal(info.head, null);
  assert.equal(info.branch, null);
  assert.equal(info.dirty, null);
});

test("parseAst returns empty list when tree-sitter is unavailable", () => {
  const ast = parseAst("/tmp/example.py", "def foo():\n    return 1\n", [
    { old_start: 1, old_lines: 0, new_start: 1, new_lines: 1 },
  ]);
  assert.ok(Array.isArray(ast.nodes));
  assert.equal(ast.ok, false);
});
