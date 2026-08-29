#!/usr/bin/env node

import { constants as fsConstants } from "node:fs";
import { access, realpath, stat } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { pathToFileURL } from "node:url";

const PI_PACKAGE = "@earendil-works/pi-coding-agent";
const INTEROP_ENV = "OTTO_PI_INTEROP";
const MAX_TEXT = 256;

function skip(message) {
  console.error(`SKIP: ${message}`);
  process.exit(77);
}

function bounded(value) {
  if (typeof value !== "string") return null;
  return value.slice(0, MAX_TEXT);
}

function boundedCount(value) {
  if (!Number.isSafeInteger(value) || value < 0) return 0;
  return value;
}

function saturatingAdd(total, delta) {
  total = boundedCount(total);
  delta = boundedCount(delta);
  const next = total + delta;
  return Number.isSafeInteger(next) ? next : Number.MAX_SAFE_INTEGER;
}

function countArray(value) {
  return Array.isArray(value) ? value.length : 0;
}

function hasOwn(object, key) {
  return Object.prototype.hasOwnProperty.call(object, key);
}

function validString(value) {
  return typeof value === "string" && value.length > 0;
}

function countCompactionEntries(entries) {
  const counts = {
    entryCount: 0,
    entriesWithFirstKeptEntryId: 0,
    entriesWithRetainedTail: 0,
    retainedTailMessageCount: 0,
    entriesWithUsage: 0,
    entriesWithDetails: 0,
    detailReadFileCount: 0,
    detailModifiedFileCount: 0,
    omittedReadFileCount: 0,
    omittedModifiedFileCount: 0,
  };

  for (const entry of entries) {
    if (entry?.type !== "compaction") continue;
    counts.entryCount = saturatingAdd(counts.entryCount, 1);

    if (!Number.isFinite(entry?.tokensBefore) || entry.tokensBefore < 0) {
      throw new Error("unexpected compaction entry metadata");
    }

    const hasFirstKeptEntryID = validString(entry?.firstKeptEntryId);
    const retainedTailPresent = hasOwn(entry ?? {}, "retainedTail");
    if (retainedTailPresent && !Array.isArray(entry?.retainedTail)) {
      throw new Error("unexpected compaction entry metadata");
    }
    const retainedTailCount = countArray(entry?.retainedTail);
    if (hasFirstKeptEntryID) {
      counts.entriesWithFirstKeptEntryId = saturatingAdd(counts.entriesWithFirstKeptEntryId, 1);
    }
    if (Array.isArray(entry?.retainedTail)) {
      counts.entriesWithRetainedTail = saturatingAdd(counts.entriesWithRetainedTail, 1);
      counts.retainedTailMessageCount = saturatingAdd(counts.retainedTailMessageCount, retainedTailCount);
    }
    if (!hasFirstKeptEntryID && !Array.isArray(entry?.retainedTail)) {
      throw new Error("compaction entry is missing a retained boundary");
    }

    if (entry?.usage && typeof entry.usage === "object") {
      counts.entriesWithUsage = saturatingAdd(counts.entriesWithUsage, 1);
    }

    const details = entry?.details;
    if (!details || typeof details !== "object") continue;
    counts.entriesWithDetails = saturatingAdd(counts.entriesWithDetails, 1);
    counts.detailReadFileCount = saturatingAdd(counts.detailReadFileCount, countArray(details.readFiles));
    counts.detailModifiedFileCount = saturatingAdd(counts.detailModifiedFileCount, countArray(details.modifiedFiles));
    counts.omittedReadFileCount = saturatingAdd(counts.omittedReadFileCount, boundedCount(details.omittedReadFiles));
    counts.omittedModifiedFileCount = saturatingAdd(counts.omittedModifiedFileCount, boundedCount(details.omittedModifiedFiles));
  }

  return counts;
}

function countCompactionContextMessages(messages) {
  const counts = {
    contextMessageCount: 0,
    contextMessagesWithTokensBefore: 0,
  };

  for (const message of messages) {
    if (bounded(message?.role) !== "compactionSummary") continue;
    if (!validString(message?.summary) || !Number.isSafeInteger(message?.tokensBefore) || message.tokensBefore < 0) {
      throw new Error("unexpected compaction context metadata");
    }
    counts.contextMessageCount = saturatingAdd(counts.contextMessageCount, 1);
    counts.contextMessagesWithTokensBefore = saturatingAdd(counts.contextMessagesWithTokensBefore, 1);
  }

  return counts;
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
if (process.env[INTEROP_ENV] !== "1") {
  skip(`${INTEROP_ENV}=1 is required to run the optional Pi interop probe`);
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

  const compactionEntries = countCompactionEntries(entries);
  const compactionContext = countCompactionContextMessages(context.messages);

  console.log(JSON.stringify({
    compatible: true,
    piVersion: bounded(pi.VERSION),
    sessionVersion: header.version,
    sessionId: bounded(manager.getSessionId()),
    cwd: bounded(manager.getCwd()),
    entryCount: entries.length,
    contextMessageCount: context.messages.length,
    contextRoles: roles,
    compaction: {
      ...compactionEntries,
      ...compactionContext,
    },
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
