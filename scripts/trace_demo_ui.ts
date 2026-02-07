#!/usr/bin/env node
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";
import { loadEntries, renderSvg } from "./trace_svg.ts";
import { buildUiData, renderHtml } from "./trace_ui.ts";

function ensureDir(dirPath: string) {
  fs.mkdirSync(dirPath, { recursive: true });
}

function writeFile(filePath: string, content: string) {
  fs.writeFileSync(filePath, content, "utf8");
}

function writeDemoConversation(conversationPath: string) {
  const lines = [
    {
      conversation_id: "conv-demo-002",
      message_id: "msg-101",
      role: "user",
      created_at: "2026-02-07T10:10:00Z",
      content: "Please add retry logic and JSON parser.",
    },
    {
      conversation_id: "conv-demo-002",
      message_id: "msg-102",
      role: "assistant",
      created_at: "2026-02-07T10:10:03Z",
      content: "Sure. I will add retryWithBackoff and parseJson in api_service.ts.",
    },
  ];
  const text = lines.map((line) => JSON.stringify(line)).join("\n") + "\n";
  writeFile(conversationPath, text);
}

function writeDemoSource(sourcePath: string) {
  const source = `export async function retryWithBackoff<T>(fn: () => Promise<T>, maxAttempts = 3): Promise<T> {
  let attempt = 0;
  let lastError: unknown;
  while (attempt < maxAttempts) {
    try {
      return await fn();
    } catch (err) {
      lastError = err;
      attempt += 1;
      await new Promise((resolve) => setTimeout(resolve, 100 * attempt));
    }
  }
  throw lastError;
}

export async function fetchData(url: string): Promise<string> {
  return retryWithBackoff(async () => {
    const resp = await fetch(url, { cache: "no-store" });
    if (!resp.ok) throw new Error(\`HTTP \${resp.status}\`);
    return await resp.text();
  });
}

export function parseJson<T>(raw: string): T {
  return JSON.parse(raw) as T;
}
`;
  writeFile(sourcePath, source);
}

function writeDemoTrace(tracePath: string) {
  const markdown = `# Agent Trace Demo Log

### 2026-02-07T10:10:31Z

Conversation: id=conv-demo-002 message_id=msg-102 role=assistant
Excerpt: Sure. I will add retryWithBackoff and parseJson in api_service.ts.
File: \`api_service.ts\`
Summary: +23 -1; function_declaration retryWithBackoff (1-14), function_declaration fetchData (16-22)

\`\`\`json
{
  "trace_entry": true,
  "schema_version": "2026-02-07",
  "timestamp": "2026-02-07T10:10:31Z",
  "conversation": {
    "id": "conv-demo-002",
    "message_id": "msg-102",
    "role": "assistant",
    "created_at": "2026-02-07T10:10:03Z",
    "excerpt": "Sure. I will add retryWithBackoff and parseJson in api_service.ts.",
    "model": "gpt-4.1",
    "latency_ms": 1840,
    "tokens_prompt": 420,
    "tokens_completion": 95,
    "cost_usd": 0.0123
  },
  "conversation_window": [
    {
      "id": "conv-demo-002",
      "message_id": "msg-101",
      "role": "user",
      "created_at": "2026-02-07T10:10:01Z"
    },
    {
      "id": "conv-demo-002",
      "message_id": "msg-102",
      "role": "assistant",
      "created_at": "2026-02-07T10:10:03Z"
    }
  ],
  "file": "api_service.ts",
  "change": {
    "added": 23,
    "deleted": 1,
    "hunks": [
      {
        "old_start": 1,
        "old_lines": 1,
        "new_start": 1,
        "new_lines": 23
      }
    ]
  },
  "ast": {
    "nodes": [
      {
        "type": "function_declaration",
        "name": "retryWithBackoff",
        "start": 1,
        "end": 14
      },
      {
        "type": "function_declaration",
        "name": "fetchData",
        "start": 16,
        "end": 22
      }
    ],
    "language": ".ts",
    "grammar_version": "0.22.6",
    "ok": true,
    "confidence": 1
  },
  "git": {
    "head": "abc123",
    "branch": "demo",
    "dirty": false
  },
  "summary": "+23 -1; function_declaration retryWithBackoff (1-14), function_declaration fetchData (16-22)"
}
\`\`\`

### 2026-02-07T10:10:34Z

Conversation: id=conv-demo-002 message_id=msg-102 role=assistant
Excerpt: Sure. I will add retryWithBackoff and parseJson in api_service.ts.
File: \`api_service.ts\`
Summary: +4 -0; function_declaration parseJson (24-26)

\`\`\`json
{
  "trace_entry": true,
  "schema_version": "2026-02-07",
  "timestamp": "2026-02-07T10:10:34Z",
  "conversation": {
    "id": "conv-demo-002",
    "message_id": "msg-102",
    "role": "assistant",
    "created_at": "2026-02-07T10:10:03Z",
    "excerpt": "Sure. I will add retryWithBackoff and parseJson in api_service.ts.",
    "model": "gpt-4.1",
    "latency_ms": 1840,
    "tokens_prompt": 420,
    "tokens_completion": 95,
    "cost_usd": 0.0123
  },
  "conversation_window": [
    {
      "id": "conv-demo-002",
      "message_id": "msg-101",
      "role": "user",
      "created_at": "2026-02-07T10:10:01Z"
    },
    {
      "id": "conv-demo-002",
      "message_id": "msg-102",
      "role": "assistant",
      "created_at": "2026-02-07T10:10:03Z"
    }
  ],
  "file": "api_service.ts",
  "change": {
    "added": 4,
    "deleted": 0,
    "hunks": [
      {
        "old_start": 24,
        "old_lines": 0,
        "new_start": 24,
        "new_lines": 4
      }
    ]
  },
  "ast": {
    "nodes": [
      {
        "type": "function_declaration",
        "name": "parseJson",
        "start": 24,
        "end": 26
      }
    ],
    "language": ".ts",
    "grammar_version": "0.22.6",
    "ok": true,
    "confidence": 1
  },
  "git": {
    "head": "abc123",
    "branch": "demo",
    "dirty": false
  },
  "summary": "+4 -0; function_declaration parseJson (24-26)"
}
\`\`\`
`;
  writeFile(tracePath, markdown);
}

function main() {
  const root = process.cwd();
  const demoDir = path.resolve(root, "docs/demo-clean");
  const demoWorkspaceDir = path.resolve(root, "demo/workspace");

  ensureDir(demoDir);
  ensureDir(demoWorkspaceDir);

  const conversationPath = path.resolve(demoDir, "conversation.jsonl");
  const tracePath = path.resolve(demoDir, "agent-trace.md");
  const svgPath = path.resolve(demoDir, "agent-trace.svg");
  const htmlPath = path.resolve(demoDir, "agent-trace.html");
  const jsonPath = path.resolve(demoDir, "agent-trace.json");
  const sourcePath = path.resolve(demoWorkspaceDir, "api_service.ts");

  writeDemoConversation(conversationPath);
  writeDemoSource(sourcePath);
  writeDemoTrace(tracePath);

  const entries = loadEntries(tracePath);
  renderSvg(entries, svgPath);
  const uiData = buildUiData(entries as any);
  writeFile(jsonPath, JSON.stringify(uiData, null, 2));
  writeFile(htmlPath, renderHtml(uiData));

  process.stdout.write(`Wrote demo conversation: ${conversationPath}\n`);
  process.stdout.write(`Wrote demo trace: ${tracePath}\n`);
  process.stdout.write(`Wrote demo svg: ${svgPath}\n`);
  process.stdout.write(`Wrote demo html: ${htmlPath}\n`);
  process.stdout.write(`Wrote demo json: ${jsonPath}\n`);
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  main();
}
