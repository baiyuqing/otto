#!/usr/bin/env node
import fs from "fs";
import path from "path";
import yargs from "yargs";
import { hideBin } from "yargs/helpers";

type TraceEntry = {
  trace_entry?: boolean;
  timestamp?: string;
  conversation?: {
    id?: string | null;
    message_id?: string | null;
    role?: string | null;
  };
  file?: string;
  summary?: string;
};

function loadEntries(logPath: string): TraceEntry[] {
  if (!fs.existsSync(logPath)) {
    return [];
  }
  const text = fs.readFileSync(logPath, "utf8");
  const entries: TraceEntry[] = [];
  let inJson = false;
  let buffer: string[] = [];
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (trimmed === "```json") {
      inJson = true;
      buffer = [];
      continue;
    }
    if (trimmed === "```" && inJson) {
      inJson = false;
      try {
        const obj = JSON.parse(buffer.join("\n"));
        if (obj && typeof obj === "object" && obj.trace_entry) {
          entries.push(obj);
        }
      } catch {
        // ignore
      }
      buffer = [];
      continue;
    }
    if (inJson) {
      buffer.push(line);
    }
  }
  return entries;
}

function sanitize(text?: string | null): string {
  if (!text) return "unknown";
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function renderSvg(entries: TraceEntry[], outPath: string) {
  if (!entries.length) {
    fs.writeFileSync(outPath, '<svg xmlns="http://www.w3.org/2000/svg" width="800" height="120"></svg>');
    return;
  }

  const convNodes: Array<TraceEntry["conversation"]> = [];
  const convIndex = new Map<string, number>();

  for (const entry of entries) {
    const conv = entry.conversation ?? {};
    const key = `${conv.id ?? ""}::${conv.message_id ?? ""}::${conv.role ?? ""}`;
    if (!convIndex.has(key)) {
      convIndex.set(key, convNodes.length);
      convNodes.push(conv);
    }
  }

  const rowHeight = 52;
  const leftX = 40;
  const rightX = 520;
  const width = 1000;
  const height = Math.max(180, (Math.max(entries.length, convNodes.length) + 1) * rowHeight);

  const parts: string[] = [];
  parts.push(`<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}">`);
  parts.push("<style>text{font-family:Arial,sans-serif;font-size:12px;}</style>");
  parts.push('<rect width="100%" height="100%" fill="#fff" />');

  parts.push(`<text x="${leftX}" y="24" fill="#111">Conversation</text>`);
  convNodes.forEach((conv, i) => {
    const y = 52 + i * rowHeight;
    const label = `${sanitize(conv?.role ?? "")} ${sanitize(conv?.message_id ?? "")}`.trim();
    parts.push(`<rect x="${leftX}" y="${y}" width="420" height="36" rx="6" fill="#f2f4f8" stroke="#cbd5e1" />`);
    parts.push(`<text x="${leftX + 10}" y="${y + 22}" fill="#111">${label || "unknown"}</text>`);
  });

  parts.push(`<text x="${rightX}" y="24" fill="#111">Change</text>`);
  entries.forEach((entry, i) => {
    const y = 52 + i * rowHeight;
    const fileLabel = sanitize(entry.file);
    const summary = sanitize(entry.summary);
    parts.push(`<rect x="${rightX}" y="${y}" width="440" height="36" rx="6" fill="#eef6ff" stroke="#93c5fd" />`);
    parts.push(`<text x="${rightX + 10}" y="${y + 16}" fill="#0f172a">${fileLabel}</text>`);
    parts.push(`<text x="${rightX + 10}" y="${y + 30}" fill="#475569">${summary}</text>`);
  });

  entries.forEach((entry, i) => {
    const conv = entry.conversation ?? {};
    const key = `${conv.id ?? ""}::${conv.message_id ?? ""}::${conv.role ?? ""}`;
    const convRow = convIndex.get(key) ?? 0;
    const y1 = 70 + convRow * rowHeight;
    const y2 = 70 + i * rowHeight;
    parts.push(`<line x1="${leftX + 420}" y1="${y1}" x2="${rightX}" y2="${y2}" stroke="#94a3b8" stroke-width="1.5" />`);
  });

  parts.push("</svg>");
  fs.writeFileSync(outPath, parts.join("\n"), "utf8");
}

function main() {
  const argv = yargs(hideBin(process.argv))
    .option("log", { type: "string", default: "docs/agent-trace.md" })
    .option("out", { type: "string", default: "docs/agent-trace.svg" })
    .parseSync();

  const logPath = path.resolve(argv.log);
  const outPath = path.resolve(argv.out);
  const entries = loadEntries(logPath);
  renderSvg(entries, outPath);
}

main();
