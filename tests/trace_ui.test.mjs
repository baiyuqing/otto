import { test } from "node:test";
import assert from "node:assert/strict";
import { buildUiData, renderHtml } from "../scripts/trace_ui.ts";

test("buildUiData groups conversation nodes and keeps change metrics", () => {
  const data = buildUiData([
    {
      trace_entry: true,
      timestamp: "2026-02-07T10:00:00Z",
      conversation: { id: "c1", message_id: "m1", role: "assistant", excerpt: "x" },
      file: "src/a.ts",
      summary: "+1 -0",
      change: { added: 1, deleted: 0 },
      ast: [{ type: "function_declaration", name: "foo", start: 1, end: 2 }],
    },
    {
      trace_entry: true,
      timestamp: "2026-02-07T10:00:01Z",
      conversation: { id: "c1", message_id: "m1", role: "assistant", excerpt: "x" },
      file: "src/b.ts",
      summary: "+2 -1",
      change: { added: 2, deleted: 1 },
      ast: [],
    },
  ]);

  assert.equal(data.conversations.length, 1);
  assert.equal(data.changes.length, 2);
  assert.equal(data.files.length, 2);
  assert.equal(data.changes[0].astCount, 1);
  assert.equal(data.changes[1].deleted, 1);
});

test("renderHtml embeds trace payload and UI anchors", () => {
  const data = buildUiData([]);
  const html = renderHtml(data);
  assert.ok(html.includes("<svg id=\"graph\""));
  assert.ok(html.includes("trace-data"));
  assert.ok(html.includes("Reset Filters"));
});
