import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { loadEntries, renderSvg, sanitize } from "../scripts/trace_svg.ts";

function writeTemp(content) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-trace-"));
  const file = path.join(dir, "agent-trace.md");
  fs.writeFileSync(file, content, "utf8");
  return { dir, file };
}

test("sanitize escapes svg-sensitive characters", () => {
  assert.equal(sanitize("a&b"), "a&amp;b");
  assert.equal(sanitize("<tag>"), "&lt;tag&gt;");
  assert.equal(sanitize(null), "unknown");
});

test("loadEntries parses JSON blocks", () => {
  const { file } = writeTemp(`### 2025-01-01T00:00:00Z\n\n\
\`\`\`json\n{\"trace_entry\":true,\"file\":\"a.ts\"}\n\`\`\`\n`);
  const entries = loadEntries(file);
  assert.equal(entries.length, 1);
  assert.equal(entries[0].file, "a.ts");
});

test("renderSvg outputs svg with labels", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-trace-"));
  const out = path.join(dir, "trace.svg");
  renderSvg(
    [
      {
        trace_entry: true,
        conversation: { id: "c1", message_id: "m1", role: "user" },
        file: "scripts/trace_watch.ts",
        summary: "+1 -0",
      },
    ],
    out,
  );
  const svg = fs.readFileSync(out, "utf8");
  assert.ok(svg.includes("<svg"));
  assert.ok(svg.includes("Conversation"));
  assert.ok(svg.includes("Change"));
});
