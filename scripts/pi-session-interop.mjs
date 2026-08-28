#!/usr/bin/env node

import { constants as fsConstants } from "node:fs";
import { access, realpath, stat } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { pathToFileURL } from "node:url";

const PI_PACKAGE = "@earendil-works/pi-coding-agent";
const MAX_TEXT = 256;

function skip(message) {
  console.error(`SKIP: ${message}`);
  process.exit(77);
}

function bounded(value) {
  if (typeof value !== "string") return null;
  return value.slice(0, MAX_TEXT);
}

async function installedPiEntry() {
  const require = createRequire(import.meta.url);
  try {
    return require.resolve(PI_PACKAGE);
  } catch {
    // A global npm installation is not in Node's normal module search path.
  }

  for (const directory of (process.env.PATH ?? "").split(path.delimiter)) {
    if (!directory) continue;
    const executable = path.join(directory, process.platform === "win32" ? "pi.cmd" : "pi");
    try {
      await access(executable, fsConstants.X_OK);
      const target = await realpath(executable);
      const packageRoot = path.resolve(path.dirname(target), "..", "..");
      const entry = path.join(packageRoot, "dist", "index.js");
      await access(entry, fsConstants.R_OK);
      return entry;
    } catch {
      // Continue searching PATH.
    }
  }
  return null;
}

if (process.argv.length !== 3) {
  console.error("usage: node scripts/pi-session-interop.mjs SESSION.jsonl");
  process.exit(2);
}

const entry = await installedPiEntry();
if (entry === null) {
  skip(`${PI_PACKAGE} is not installed or its pi executable is not on PATH`);
}

let pi;
try {
  pi = await import(pathToFileURL(entry).href);
} catch {
  skip(`${PI_PACKAGE} could not be imported`);
}
if (typeof pi.SessionManager?.open !== "function" || pi.CURRENT_SESSION_VERSION !== 3) {
  skip(`${PI_PACKAGE} does not expose the Pi v3 SessionManager API`);
}

const sessionPath = path.resolve(process.argv[2]);
try {
  const info = await stat(sessionPath);
  if (!info.isFile()) throw new Error("not a regular file");

  const manager = pi.SessionManager.open(sessionPath);
  const header = manager.getHeader();
  const entries = manager.getEntries();
  const context = manager.buildSessionContext();
  if (!header || header.version !== 3 || !Array.isArray(entries) || !Array.isArray(context.messages)) {
    throw new Error("unexpected Pi session state");
  }

  const roles = {};
  for (const message of context.messages) {
    const role = bounded(message?.role) ?? "unknown";
    roles[role] = (roles[role] ?? 0) + 1;
  }

  console.log(JSON.stringify({
    compatible: true,
    piVersion: bounded(pi.VERSION),
    sessionVersion: header.version,
    sessionId: bounded(manager.getSessionId()),
    cwd: bounded(manager.getCwd()),
    entryCount: entries.length,
    contextMessageCount: context.messages.length,
    contextRoles: roles,
    model: context.model ? {
      provider: bounded(context.model.provider),
      id: bounded(context.model.modelId),
    } : null,
    thinkingLevel: bounded(context.thinkingLevel),
  }));
} catch {
  console.error("invalid session: Pi could not open the file and build its context");
  process.exit(1);
}
