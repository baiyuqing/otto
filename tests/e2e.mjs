import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function getNodeBin() {
  return process.execPath;
}

function readIfExists(filePath) {
  try {
    return fs.readFileSync(filePath, "utf8");
  } catch {
    return "";
  }
}

function waitFor(predicate, timeoutMs = 5000, intervalMs = 200) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tick = async () => {
      if (await predicate()) {
        resolve(true);
        return;
      }
      if (Date.now() - started > timeoutMs) {
        reject(new Error("Timed out waiting for condition"));
        return;
      }
      setTimeout(tick, intervalMs);
    };
    tick();
  });
}

test("end-to-end: watch -> log -> svg", async () => {
  const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), "agent-trace-"));
  const docsDir = path.join(tmpRoot, "docs");
  fs.mkdirSync(docsDir, { recursive: true });

  const conversationLog = path.join(tmpRoot, "conversation.jsonl");
  fs.writeFileSync(
    conversationLog,
    JSON.stringify({
      conversation_id: "conv-1",
      message_id: "msg-1",
      role: "user",
      created_at: new Date().toISOString(),
      content: "Please track changes",
    }) + "\n",
    "utf8",
  );

  const traceLog = path.join(tmpRoot, "docs", "agent-trace.md");
  const stateFile = path.join(tmpRoot, ".trace_state.json");

  const nodeBin = getNodeBin();
  const watcher = spawn(
    nodeBin,
    [
      "--import",
      "tsx",
      "scripts/trace_watch.ts",
      "--root",
      tmpRoot,
      "--log",
      traceLog,
      "--conversation-log",
      conversationLog,
      "--state",
      stateFile,
      "--debounce-ms",
      "200",
    ],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        CHOKIDAR_USEPOLLING: "1",
        CHOKIDAR_INTERVAL: "100",
      },
    },
  );

  let watcherExited = false;
  let watcherStdErr = "";
  let watcherStdOut = "";
  watcher.stderr.on("data", (chunk) => {
    watcherStdErr += String(chunk);
  });
  watcher.stdout.on("data", (chunk) => {
    watcherStdOut += String(chunk);
  });
  watcher.on("exit", () => {
    watcherExited = true;
  });

  await sleep(500);

  const sampleFile = path.join(tmpRoot, "sample.py");
  fs.writeFileSync(sampleFile, "def foo():\n    return 1\n", "utf8");
  await sleep(500);
  fs.appendFileSync(sampleFile, "\n# change\n", "utf8");

  await waitFor(() => {
    if (watcherExited) {
      throw new Error(`watcher exited early\\nstdout:\\n${watcherStdOut}\\nstderr:\\n${watcherStdErr}`);
    }
    return readIfExists(traceLog).includes("\"trace_entry\": true");
  }, 12000);

  const svgPath = path.join(tmpRoot, "docs", "agent-trace.svg");
  const svgRun = spawn(
    nodeBin,
    ["--import", "tsx", "scripts/trace_svg.ts", "--log", traceLog, "--out", svgPath],
    {
      stdio: "ignore",
    },
  );

  await new Promise((resolve, reject) => {
    svgRun.on("exit", (code) => {
      if (code === 0) resolve(true);
      else reject(new Error(`trace_svg.ts exited with ${code}`));
    });
  });

  const svg = readIfExists(svgPath);
  assert.ok(svg.includes("<svg"), "SVG output missing <svg tag>");
  assert.ok(svg.includes("Conversation"), "SVG output missing Conversation label");
  assert.ok(svg.includes("Change"), "SVG output missing Change label");

  watcher.kill("SIGINT");
  await new Promise((resolve) => {
    const timeout = setTimeout(() => {
      watcher.kill("SIGKILL");
      resolve(true);
    }, 1000);
    watcher.on("exit", () => {
      clearTimeout(timeout);
      resolve(true);
    });
  });
});
