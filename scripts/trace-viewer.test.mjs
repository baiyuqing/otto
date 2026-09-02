// Checks the pure parsing helpers in trace-viewer.html against both provider
// wire shapes: chat-completions (openaicompat) and the Responses API
// (openairesponses). Opt-in, like scripts/pi-session-interop.test.mjs:
//
//   node --test scripts/trace-viewer.test.mjs
//
// The helpers live between the "pure parsing" markers in the HTML so this file
// can evaluate them without a DOM.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const html = readFileSync(fileURLToPath(new URL("./trace-viewer.html", import.meta.url)), "utf8");
const block = html.match(/\/\/ --- pure parsing start ---\n([\s\S]*?)\n\/\/ --- pure parsing end ---/);
assert.ok(block, "trace-viewer.html must delimit its pure parsing helpers with the marker comments");
const { parseSSE, requestMessages, requestSystem, msgLabel } = new Function(
  `${block[1]}\nreturn { parseSSE, requestMessages, requestSystem, msgLabel };`,
)();

const sse = (objs) => objs.map((o) => `data: ${JSON.stringify(o)}\n`).join("\n") + "\ndata: [DONE]\n\n";

test("parseSSE reads chat-completions deltas and usage", () => {
  const raw = sse([
    { choices: [{ delta: { content: "he" } }] },
    { choices: [{ delta: { content: "llo" } }] },
    { choices: [{ delta: {} }], usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 } },
  ]);
  const { text, usage } = parseSSE(raw);
  assert.equal(text, "hello");
  assert.equal(usage.total_tokens, 5);
});

test("parseSSE reads Responses API deltas and usage", () => {
  const raw = sse([
    { type: "response.created", response: { id: "resp_1", status: "in_progress" } },
    { type: "response.output_text.delta", delta: "he" },
    { type: "response.output_text.delta", delta: "llo" },
    { type: "response.output_text.done", text: "hello" },
    {
      type: "response.completed",
      response: { id: "resp_1", usage: { input_tokens: 3, output_tokens: 2, total_tokens: 5 } },
    },
  ]);
  const { text, usage } = parseSSE(raw);
  assert.equal(text, "hello");
  assert.equal(usage.total_tokens, 5);
});

test("parseSSE surfaces Responses API tool calls", () => {
  const raw = sse([
    {
      type: "response.output_item.done",
      item: { type: "function_call", name: "read", arguments: '{"path":"AGENTS.md"}' },
    },
  ]);
  const { text } = parseSSE(raw);
  assert.match(text, /read/);
  assert.match(text, /AGENTS\.md/);
});

test("parseSSE prints the tool call_id", () => {
  const raw = sse([
    {
      type: "response.output_item.done",
      item: { type: "function_call", name: "ls", arguments: "{}", call_id: "call_abc", id: "fc_1" },
    },
  ]);
  assert.match(parseSSE(raw).text, /call_abc/);
});

test("requestMessages and requestSystem cover both request shapes", () => {
  const chat = { messages: [{ role: "user", content: "hi" }], system: "sys" };
  assert.equal(requestMessages(chat).length, 1);
  assert.equal(requestSystem(chat), "sys");

  const responses = {
    instructions: "sys",
    input: [
      { type: "message", role: "user", content: [{ type: "input_text", text: "hi" }] },
      { type: "function_call_output", call_id: "c1", output: "done" },
    ],
  };
  const msgs = requestMessages(responses);
  assert.equal(msgs.length, 2);
  assert.equal(msgs[0].role, "user");
  assert.equal(msgs[1].role, "function_call_output");
  assert.equal(requestSystem(responses), "sys");
});

test("msgLabel carries the call_id of tool traffic in both shapes", () => {
  const responses = requestMessages({
    input: [
      { type: "function_call", name: "ls", call_id: "call_abc", arguments: "{}" },
      { type: "function_call_output", call_id: "call_abc", output: "done" },
    ],
  });
  assert.match(msgLabel(responses[0]), /call_abc/);
  assert.match(msgLabel(responses[1]), /call_abc/);

  const chat = requestMessages({
    messages: [
      { role: "assistant", tool_calls: [{ id: "call_xyz", function: { name: "ls" } }] },
      { role: "tool", tool_call_id: "call_xyz", content: "done" },
    ],
  });
  assert.match(msgLabel(chat[0]), /call_xyz/);
  assert.match(msgLabel(chat[1]), /call_xyz/);
  assert.equal(msgLabel({ role: "user", content: "hi" }), "user");
});
