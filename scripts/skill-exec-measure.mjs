#!/usr/bin/env node
/**
 * Measures what `docs/specs/2026-09-22-skill-subagent-execution.md` says must
 * be measured before sub-agent execution of skills is trusted: peak context
 * per window, total tokens, and how often the model honours the
 * `exec="agent"` marking.
 *
 * Two sources are needed and neither replaces the other. A sub-agent's
 * transcript is a `MemorySession` and never reaches disk, so its context peak
 * exists only in the usage database, tagged with its task id. The routing
 * decision, in contrast, is only visible in the parent's transcript, because
 * that is where the model chose between the `skill` and `agent` tools.
 *
 * Reading only: it opens the usage database read-only and never writes.
 *
 *   node scripts/skill-exec-measure.mjs <session.jsonl> [--usage <path>]
 */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";

/** The `skill` tool call that loads instructions inline, not one reading a file. */
const isInlineLoad = (call) => call.tool === "skill" && !call.arguments?.file;

/**
 * Reads one Pi v3 transcript.
 *
 * `peakInput` is the largest single request's context, which is the quantity
 * the design compares; summing would measure something else entirely. A line
 * that does not parse is skipped, so a truncated tail cannot lose the rest.
 */
export function parseSession(text) {
  let sessionId = "";
  let peakInput = 0;
  const totals = { input: 0, output: 0 };
  const routing = {};

  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    let record;
    try {
      record = JSON.parse(line);
    } catch {
      continue;
    }
    if (record.type === "session") {
      sessionId = record.id ?? "";
      continue;
    }
    const message = record.message;
    if (!message) continue;

    const usage = message.usage;
    if (usage) {
      const input = Number(usage.input) || 0;
      peakInput = Math.max(peakInput, input);
      totals.input += input;
      totals.output += Number(usage.output) || 0;
    }

    for (const block of message.content ?? []) {
      if (block.type !== "toolCall") continue;
      const call = { tool: block.name, arguments: block.arguments ?? {} };
      const name =
        call.tool === "skill"
          ? call.arguments.name
          : call.tool === "agent"
            ? call.arguments.agent
            : undefined;
      if (!name) continue;
      routing[name] ??= { inline: 0, delegated: 0 };
      if (isInlineLoad(call)) routing[name].inline += 1;
      if (call.tool === "agent") routing[name].delegated += 1;
    }
  }

  return { sessionId, peakInput, totals, routing };
}

/**
 * The context peak of every window one session used, main context first.
 *
 * Rows come from `usage_events` as `crates/kite/src/usage.rs` writes them: an
 * empty `task_id` is the main context and every other value is one sub-agent.
 */
export function windowPeaks(database, sessionId) {
  const { DatabaseSync } = require_sqlite();
  const connection = new DatabaseSync(database, { readOnly: true });
  try {
    return connection
      .prepare(
        `SELECT task_id, MAX(input_tokens) AS peak, COUNT(*) AS requests
           FROM usage_events
          WHERE session_id = ? AND usage_present = 1
          GROUP BY task_id
          ORDER BY (task_id <> ''), task_id`,
      )
      .all(sessionId)
      .map((row) => ({
        window: row.task_id === "" ? "main" : row.task_id,
        peakInput: Number(row.peak),
        requests: Number(row.requests),
      }));
  } finally {
    connection.close();
  }
}

/**
 * Loaded on demand, not at import: `node:sqlite` is experimental and warns on
 * import, and the transcript half of this script needs no database at all.
 * The script only ever reads, so the exposure is a future API change rather
 * than the data.
 */
function require_sqlite() {
  return createRequire(import.meta.url)("node:sqlite");
}

/** Renders one session's numbers. */
export function report(parsed, peaks) {
  const across = Math.max(parsed.peakInput, ...peaks.map((peak) => peak.peakInput), 0);
  const lines = [
    `session ${parsed.sessionId || "(unknown)"}`,
    `  peak across all windows : ${across}`,
    `  total tokens            : ${parsed.totals.input + parsed.totals.output}` +
      ` (in ${parsed.totals.input}, out ${parsed.totals.output})`,
    "  windows:",
  ];
  for (const peak of peaks) {
    lines.push(`    ${peak.window.padEnd(22)} peak ${peak.peakInput} over ${peak.requests} requests`);
  }
  const names = Object.keys(parsed.routing).sort();
  lines.push(names.length ? "  routing:" : "  routing: none observed");
  for (const name of names) {
    const { inline, delegated } = parsed.routing[name];
    lines.push(`    ${name.padEnd(22)} inline ${inline}, delegated ${delegated}`);
  }
  return lines.join("\n");
}

function main(argv) {
  const sessions = [];
  let database = path.join(os.homedir(), ".kite/usage.db");
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === "--usage") {
      database = argv[index + 1];
      index += 1;
    } else {
      sessions.push(argv[index]);
    }
  }
  if (sessions.length === 0) {
    console.error("usage: skill-exec-measure.mjs <session.jsonl>... [--usage <path>]");
    return 2;
  }
  for (const session of sessions) {
    const parsed = parseSession(readFileSync(session, "utf8"));
    let peaks = [];
    try {
      peaks = windowPeaks(database, parsed.sessionId);
    } catch (error) {
      console.error(`warning: ${database}: ${error.message}; sub-agent windows omitted`);
    }
    console.log(report(parsed, peaks));
  }
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = main(process.argv.slice(2));
}
