#!/usr/bin/env node
import fs from "fs";
import path from "path";
import process from "process";
import { spawn } from "node-pty";
import stripAnsi from "strip-ansi";

type LogEntry = {
  conversation_id: string;
  message_id: string;
  role: "user" | "assistant" | "system";
  created_at: string;
  content: string;
};

function utcNow() {
  return new Date().toISOString();
}

function writeEntry(logPath: string, entry: LogEntry) {
  fs.appendFileSync(logPath, JSON.stringify(entry) + "\n", "utf8");
}

function getLogPath() {
  const envPath = process.env.CODEX_CONV_LOG;
  if (envPath) return path.resolve(envPath);
  return path.resolve(process.cwd(), "docs", "conversation.jsonl");
}

function getConversationId() {
  return process.env.CODEX_CONV_ID ?? `conv-${Date.now().toString(36)}`;
}

function main() {
  const logPath = getLogPath();
  fs.mkdirSync(path.dirname(logPath), { recursive: true });

  const conversationId = getConversationId();
  const codexArgs = process.argv.slice(2);
  const args = ["--no-alt-screen", ...codexArgs];

  const pty = spawn("codex", args, {
    name: "xterm-256color",
    cols: process.stdout.columns || 80,
    rows: process.stdout.rows || 24,
    cwd: process.cwd(),
    env: process.env,
  });

  process.stdout.write(`Logging conversation to ${logPath}\n`);

  let userBuffer = "";
  let assistantBuffer = "";
  let assistantTimer: NodeJS.Timeout | null = null;

  const flushAssistant = () => {
    if (!assistantBuffer.trim()) {
      assistantBuffer = "";
      return;
    }
    writeEntry(logPath, {
      conversation_id: conversationId,
      message_id: `msg-${Date.now().toString(36)}-a`,
      role: "assistant",
      created_at: utcNow(),
      content: assistantBuffer.trim(),
    });
    assistantBuffer = "";
  };

  const scheduleAssistantFlush = () => {
    if (assistantTimer) {
      clearTimeout(assistantTimer);
    }
    assistantTimer = setTimeout(flushAssistant, 600);
  };

  pty.onData((data) => {
    process.stdout.write(data);
    const clean = stripAnsi(data);
    assistantBuffer += clean;
    scheduleAssistantFlush();
  });

  if (process.stdin.isTTY) {
    process.stdin.setRawMode(true);
  }
  process.stdin.resume();

  process.stdin.on("data", (data) => {
    const text = data.toString("utf8");
    pty.write(text);
    for (const ch of text) {
      if (ch === "\r" || ch === "\n") {
        const content = userBuffer.trim();
        if (content) {
          writeEntry(logPath, {
            conversation_id: conversationId,
            message_id: `msg-${Date.now().toString(36)}-u`,
            role: "user",
            created_at: utcNow(),
            content,
          });
        }
        userBuffer = "";
      } else if (ch === "\u0003") {
        // Ctrl+C
        pty.kill();
      } else if (ch === "\u007f") {
        // Backspace
        userBuffer = userBuffer.slice(0, -1);
      } else if (ch >= " " && ch !== "\u001b") {
        userBuffer += ch;
      }
    }
  });

  pty.onExit(() => {
    if (assistantTimer) {
      clearTimeout(assistantTimer);
    }
    flushAssistant();
    process.exit(0);
  });

  process.stdout.on("resize", () => {
    pty.resize(process.stdout.columns || 80, process.stdout.rows || 24);
  });
}

main();
