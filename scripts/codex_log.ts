#!/usr/bin/env node
import fs from "fs";
import path from "path";
import process from "process";
import { spawn as spawnPty } from "node-pty";
import stripAnsi from "strip-ansi";
import { execSync } from "child_process";
import { spawn as spawnChild } from "child_process";

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

function resolveCodexCommand(baseArgs: string[]) {
  if (process.env.CODEX_BIN && process.env.CODEX_BIN.trim()) {
    const bin = process.env.CODEX_BIN.trim();
    return { file: bin, args: baseArgs };
  }
  try {
    const resolved = execSync("command -v codex", {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
    if (resolved) {
      const target = fs.realpathSync(resolved);
      if (target.endsWith(".js")) {
        return { file: process.execPath, args: [target, ...baseArgs] };
      }
      return { file: resolved, args: baseArgs };
    }
  } catch {
    // fall through to explicit error
  }
  return null;
}

function main() {
  const logPath = getLogPath();
  fs.mkdirSync(path.dirname(logPath), { recursive: true });

  const conversationId = getConversationId();
  const codexArgs = process.argv.slice(2);
  const command = resolveCodexCommand(["--no-alt-screen", ...codexArgs]);

  if (!command) {
    process.stderr.write(
      "Cannot find `codex` in PATH. Set CODEX_BIN to the absolute codex binary path, for example:\n" +
        "CODEX_BIN=/absolute/path/to/codex npm run codex:log\n",
    );
    process.exit(1);
  }

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

  let pty: ReturnType<typeof spawnPty> | null = null;
  let child: ReturnType<typeof spawnChild> | null = null;
  let passthroughMode = false;

  try {
    pty = spawnPty(command.file, command.args, {
      name: "xterm-256color",
      cols: process.stdout.columns || 80,
      rows: process.stdout.rows || 24,
      cwd: process.cwd(),
      env: process.env,
    });
  } catch {
    passthroughMode = true;
    child = spawnChild(command.file, command.args, {
      cwd: process.cwd(),
      env: process.env,
      stdio: "inherit",
    });
    process.stderr.write(
      "[agent-trace] node-pty is unavailable in this runtime; using passthrough mode.\n" +
        "[agent-trace] To restore full conversation capture, use Node 20/22 and run: npm rebuild node-pty\n",
    );
  }

  if (pty) {
    pty.onData((data) => {
      process.stdout.write(data);
      const clean = stripAnsi(data);
      assistantBuffer += clean;
      scheduleAssistantFlush();
    });
  }

  if (process.stdin.isTTY && !passthroughMode) {
    process.stdin.setRawMode(true);
  }
  process.stdin.resume();

  process.stdin.on("data", (data) => {
    const text = data.toString("utf8");
    if (passthroughMode) {
      if (text.includes("\u0003") && child) {
        child.kill("SIGINT");
      }
      return;
    }
    if (pty) {
      pty.write(text);
    } else if (child?.stdin) {
      child.stdin.write(text);
    }
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
        if (pty) {
          pty.kill();
        } else if (child) {
          child.kill("SIGINT");
        }
      } else if (ch === "\u007f") {
        // Backspace
        userBuffer = userBuffer.slice(0, -1);
      } else if (ch >= " " && ch !== "\u001b") {
        userBuffer += ch;
      }
    }
  });

  const cleanupAndExit = (code = 0) => {
    if (assistantTimer) {
      clearTimeout(assistantTimer);
    }
    flushAssistant();
    process.exit(code);
  };

  if (pty) {
    pty.onExit(() => {
      cleanupAndExit(0);
    });
  } else if (child) {
    child.on("exit", (code) => {
      cleanupAndExit(code ?? 0);
    });
  }

  process.stdout.on("resize", () => {
    if (pty) {
      pty.resize(process.stdout.columns || 80, process.stdout.rows || 24);
    }
  });
}

main();
