#!/usr/bin/env node
/// <reference types="node" />
import chokidar from "chokidar";
import crypto from "crypto";
import fs from "fs";
import path from "path";
import process from "process";
import yargs from "yargs";
import { hideBin } from "yargs/helpers";
import { createRequire } from "module";
import { fileURLToPath } from "url";

type ChangeHunk = {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
};

type TraceEntry = {
  trace_entry: true;
  timestamp: string;
  conversation: {
    id: string | null;
    message_id: string | null;
    role: string | null;
    created_at: string | null;
    excerpt: string | null;
  };
  file: string;
  change: { added: number; deleted: number; hunks: ChangeHunk[] };
  ast: Array<{ type: string; name: string | null; start: number; end: number }>;
  summary: string;
};

const DEFAULT_EXCLUDES = new Set([
  ".git",
  ".venv",
  "venv",
  "node_modules",
  ".pytest_cache",
  "docs/agent-trace.md",
  ".trace_state.json",
]);

const require = createRequire(import.meta.url);

type TreeSitterBundle = {
  Parser: any;
  languages: Record<string, any>;
};

let treeSitterBundle: TreeSitterBundle | null | undefined;

function loadTreeSitter(): TreeSitterBundle | null {
  if (treeSitterBundle !== undefined) {
    return treeSitterBundle;
  }
  try {
    const Parser = require("tree-sitter");
    const Python = require("tree-sitter-python");
    const Go = require("tree-sitter-go");
    const JavaScript = require("tree-sitter-javascript");
    const TypeScript = require("tree-sitter-typescript");
    treeSitterBundle = {
      Parser,
      languages: {
        ".py": Python,
        ".pyi": Python,
        ".ts": TypeScript.typescript,
        ".tsx": TypeScript.tsx,
        ".js": JavaScript,
        ".jsx": JavaScript,
        ".go": Go,
      },
    };
  } catch {
    treeSitterBundle = null;
  }
  return treeSitterBundle;
}

export function utcNow(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
}

export function sha256(text: string): string {
  return crypto.createHash("sha256").update(text, "utf8").digest("hex");
}

function isExcluded(filePath: string, root: string): boolean {
  const rel = path.relative(root, filePath).replace(/\\/g, "/");
  const parts = rel.split("/");
  for (const part of parts) {
    if (DEFAULT_EXCLUDES.has(part)) {
      return true;
    }
  }
  for (const prefix of DEFAULT_EXCLUDES) {
    if (rel.startsWith(prefix)) {
      return true;
    }
  }
  return false;
}

function loadState(statePath: string): Record<string, { hash: string; content: string; updated_at: string }> {
  if (!fs.existsSync(statePath)) {
    return {};
  }
  try {
    return JSON.parse(fs.readFileSync(statePath, "utf8"));
  } catch {
    return {};
  }
}

function saveState(statePath: string, state: Record<string, { hash: string; content: string; updated_at: string }>) {
  fs.writeFileSync(statePath, JSON.stringify(state, null, 2));
}

export function getLatestConversation(logPath?: string) {
  if (!logPath || !fs.existsSync(logPath)) {
    return {
      id: null,
      message_id: null,
      role: null,
      created_at: null,
      excerpt: null,
    };
  }
  const lines = fs.readFileSync(logPath, "utf8").split(/\r?\n/);
  let latest: any = null;
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      latest = JSON.parse(trimmed);
    } catch {
      continue;
    }
  }
  if (!latest || typeof latest !== "object") {
    return {
      id: null,
      message_id: null,
      role: null,
      created_at: null,
      excerpt: null,
    };
  }
  let content = latest.content;
  if (Array.isArray(content)) {
    content = content.map(String).join(" ");
  }
  if (typeof content !== "string") {
    content = "";
  }
  const excerpt = content.trim().replace(/\n/g, " ").slice(0, 140) || null;
  return {
    id: latest.conversation_id ?? latest.id ?? null,
    message_id: latest.message_id ?? latest.id ?? null,
    role: latest.role ?? null,
    created_at: latest.created_at ?? latest.timestamp ?? null,
    excerpt,
  };
}

export function parseDiff(oldText: string, newText: string) {
  const oldLines = oldText.split(/\r?\n/);
  const newLines = newText.split(/\r?\n/);
  let oldLine = 1;
  let newLine = 1;
  let added = 0;
  let deleted = 0;
  const hunks: ChangeHunk[] = [];

  let hunkOldStart = 0;
  let hunkNewStart = 0;
  let hunkOldLines = 0;
  let hunkNewLines = 0;

  const max = Math.max(oldLines.length, newLines.length);

  function flushHunk() {
    if (hunkOldLines > 0 || hunkNewLines > 0) {
      hunks.push({
        old_start: hunkOldStart,
        old_lines: hunkOldLines,
        new_start: hunkNewStart,
        new_lines: hunkNewLines,
      });
      hunkOldStart = 0;
      hunkNewStart = 0;
      hunkOldLines = 0;
      hunkNewLines = 0;
    }
  }

  for (let i = 0; i < max; i += 1) {
    const oldLineText = oldLines[i];
    const newLineText = newLines[i];
    if (oldLineText === newLineText) {
      flushHunk();
      if (oldLineText !== undefined) oldLine += 1;
      if (newLineText !== undefined) newLine += 1;
      continue;
    }

    if (hunkOldStart === 0 && hunkNewStart === 0) {
      hunkOldStart = oldLine;
      hunkNewStart = newLine;
    }

    if (oldLineText !== undefined) {
      hunkOldLines += 1;
      deleted += 1;
      oldLine += 1;
    }
    if (newLineText !== undefined) {
      hunkNewLines += 1;
      added += 1;
      newLine += 1;
    }
  }

  flushHunk();

  return { added, deleted, hunks };
}

export function parseAst(filePath: string, newText: string, hunks: ChangeHunk[]) {
  const bundle = loadTreeSitter();
  if (!bundle) {
    return [] as Array<{ type: string; name: string | null; start: number; end: number }>;
  }
  const ext = path.extname(filePath);
  const language = bundle.languages[ext];
  if (!language) {
    return [] as Array<{ type: string; name: string | null; start: number; end: number }>;
  }
  const parser = new bundle.Parser();
  parser.setLanguage(language);
  const tree = parser.parse(newText);

  const ranges = hunks.map((h) => {
    const start = h.new_start ?? 1;
    const length = Math.max(h.new_lines ?? 0, 1);
    return [start, start + length - 1] as const;
  });

  if (!ranges.length) {
    return [] as Array<{ type: string; name: string | null; start: number; end: number }>;
  }

  function intersects(start: number, end: number) {
    return ranges.some(([rStart, rEnd]) => !(end < rStart || start > rEnd));
  }

  function getName(node: any): string | null {
    for (const child of node.namedChildren) {
      if (["identifier", "name", "type_identifier"].includes(child.type)) {
        return newText.slice(child.startIndex, child.endIndex);
      }
    }
    return null;
  }

  const results: Array<{ type: string; name: string | null; start: number; end: number }> = [];

  function walk(node: any) {
    const start = node.startPosition.row + 1;
    const end = node.endPosition.row + 1;
    if (intersects(start, end)) {
      results.push({
        type: node.type,
        name: getName(node),
        start,
        end,
      });
    }
    for (const child of node.namedChildren) {
      walk(child);
    }
  }

  walk(tree.rootNode);

  const unique = new Map<string, { type: string; name: string | null; start: number; end: number }>();
  for (const item of results) {
    const key = `${item.type}:${item.name ?? ""}:${item.start}:${item.end}`;
    if (!unique.has(key)) {
      unique.set(key, item);
    }
  }
  return Array.from(unique.values()).slice(0, 12);
}

export function summarizeNodes(nodes: Array<{ type: string; name: string | null; start: number; end: number }>) {
  if (!nodes.length) return "no AST matches";
  const parts: string[] = [];
  for (const node of nodes.slice(0, 3)) {
    let label = node.type;
    if (node.name) {
      label += ` ${node.name}`;
    }
    label += ` (${node.start}-${node.end})`;
    parts.push(label);
  }
  if (nodes.length > 3) {
    parts.push("...");
  }
  return parts.join(", ");
}

function appendMarkdown(logPath: string, entry: TraceEntry) {
  const conv = entry.conversation;
  const lines: string[] = [];
  lines.push("");
  lines.push(`### ${entry.timestamp}`);
  lines.push(`Conversation: id=${conv.id} message_id=${conv.message_id} role=${conv.role}`);
  if (conv.excerpt) {
    lines.push(`Excerpt: ${conv.excerpt}`);
  }
  lines.push(`File: \`${entry.file}\``);
  lines.push(`Summary: ${entry.summary}`);
  lines.push("```json");
  lines.push(JSON.stringify(entry, null, 2));
  lines.push("```");
  fs.appendFileSync(logPath, lines.join("\n"), "utf8");
}

function buildEntry(root: string, filePath: string, oldText: string, newText: string, conv: TraceEntry["conversation"]): TraceEntry {
  const change = parseDiff(oldText, newText);
  const nodes = parseAst(filePath, newText, change.hunks);
  const summary = `+${change.added} -${change.deleted}; ${summarizeNodes(nodes)}`;
  return {
    trace_entry: true,
    timestamp: utcNow(),
    conversation: conv,
    file: path.relative(root, filePath).replace(/\\/g, "/"),
    change,
    ast: nodes,
    summary,
  };
}

function main() {
  const argv = yargs(hideBin(process.argv))
    .option("root", { type: "string", default: "." })
    .option("log", { type: "string", default: "docs/agent-trace.md" })
    .option("conversation-log", { type: "string", default: undefined })
    .option("state", { type: "string", default: ".trace_state.json" })
    .option("debounce-ms", { type: "number", default: 800 })
    .parseSync();

  const root = path.resolve(argv.root);
  const logPath = path.resolve(root, argv.log);
  const statePath = path.resolve(root, argv.state);
  const conversationLog = argv["conversation-log"]
    ? path.resolve(String(argv["conversation-log"]))
    : undefined;

  fs.mkdirSync(path.dirname(logPath), { recursive: true });

  const state = loadState(statePath);
  const timers = new Map<string, NodeJS.Timeout>();

  const watcher = chokidar.watch(root, {
    ignored: (watchPath) => isExcluded(String(watchPath), root),
    ignoreInitial: true,
    persistent: true,
  });

  function schedule(filePath: string) {
    if (isExcluded(filePath, root)) return;
    if (timers.has(filePath)) {
      clearTimeout(timers.get(filePath));
    }
    const timeout = setTimeout(() => {
      timers.delete(filePath);
      if (!fs.existsSync(filePath)) return;
      const rel = path.relative(root, filePath).replace(/\\/g, "/");
      let newText = "";
      try {
        newText = fs.readFileSync(filePath, "utf8");
      } catch {
        return;
      }
      const previous = state[rel];
      const oldText = previous?.content ?? "";
      const newHash = sha256(newText);
      if (previous && previous.hash === newHash) {
        return;
      }
      const conv = getLatestConversation(conversationLog);
      const entry = buildEntry(root, filePath, oldText, newText, conv);
      appendMarkdown(logPath, entry);
      state[rel] = { hash: newHash, content: newText, updated_at: utcNow() };
      saveState(statePath, state);
    }, argv["debounce-ms"]);
    timers.set(filePath, timeout);
  }

  watcher.on("add", schedule);
  watcher.on("change", schedule);

  process.on("SIGINT", () => {
    watcher.close();
    process.exit(0);
  });
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  main();
}
