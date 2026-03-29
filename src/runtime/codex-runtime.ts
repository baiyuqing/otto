import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { StubRuntimeAdapter } from "./adapter.js";
import type { ExecuteTurnInput, RuntimeEvent, RuntimeSessionRef } from "./types.js";

export interface CodexRuntimeAdapterOptions {
  executablePath?: string;
  sandboxMode?: "read-only" | "workspace-write" | "danger-full-access";
  model?: string;
  timeoutMs?: number;
}

function buildCodexPrompt(input: ExecuteTurnInput): string {
  return [
    "## Framework System",
    input.prompt.system.trim(),
    "",
    "## User Task",
    input.prompt.user.trim(),
  ].join("\n");
}

function createFailureEvent(message: string): RuntimeEvent {
  return {
    type: "failed",
    error: {
      code: "codex_exec_failed",
      message,
    },
  };
}

export class CodexRuntimeAdapter extends StubRuntimeAdapter {
  readonly adapterId = "local:codex";

  readonly capabilities = {
    canResumeSession: false,
    canInterrupt: false,
    supportsToolStreaming: false,
    supportsStructuredOutput: false,
  } as const;

  readonly descriptor = {
    target: "codex",
    adapterId: this.adapterId,
    family: "codex",
    label: "Local Codex",
    transport: "local",
    capabilities: this.capabilities,
  } as const;

  private readonly executablePath: string;
  private readonly sandboxMode: "read-only" | "workspace-write" | "danger-full-access";
  private readonly model: string | undefined;
  private readonly timeoutMs: number;

  constructor(options: CodexRuntimeAdapterOptions = {}) {
    super();
    this.executablePath = options.executablePath ?? "codex";
    this.sandboxMode = options.sandboxMode ?? "workspace-write";
    this.model = options.model;
    this.timeoutMs = options.timeoutMs ?? 60_000;
  }

  async *executeTurn(input: ExecuteTurnInput): AsyncIterable<RuntimeEvent> {
    const tempDir = await mkdtemp(join(tmpdir(), "otto-codex-exec-"));
    const outputPath = join(tempDir, "last-message.txt");
    const prompt = buildCodexPrompt(input);
    const args = [
      "exec",
      "--json",
      "-o",
      outputPath,
      "--sandbox",
      this.sandboxMode,
      "-C",
      input.session.workspacePath,
      "-",
    ];

    if (this.model) {
      args.unshift(this.model);
      args.unshift("--model");
    }

    try {
      const result = await new Promise<{
        exitCode: number | null;
        stdout: string;
        stderr: string;
        timedOut: boolean;
      }>((resolvePromise, rejectPromise) => {
        const child = spawn(this.executablePath, args, {
          cwd: input.session.workspacePath,
          stdio: ["pipe", "pipe", "pipe"],
        });

        let stdout = "";
        let stderr = "";
        let settled = false;
        const timeout = setTimeout(() => {
          if (settled) {
            return;
          }

          settled = true;
          child.kill("SIGKILL");
          resolvePromise({
            exitCode: null,
            stdout,
            stderr,
            timedOut: true,
          });
        }, this.timeoutMs);

        child.stdout.setEncoding("utf8");
        child.stderr.setEncoding("utf8");

        child.stdout.on("data", (chunk: string) => {
          stdout += chunk;
        });

        child.stderr.on("data", (chunk: string) => {
          stderr += chunk;
        });

        child.on("error", (error) => {
          if (settled) {
            return;
          }

          settled = true;
          clearTimeout(timeout);
          rejectPromise(error);
        });
        child.on("close", (exitCode) => {
          if (settled) {
            return;
          }

          settled = true;
          clearTimeout(timeout);
          resolvePromise({ exitCode, stdout, stderr, timedOut: false });
        });

        child.stdin.write(prompt);
        child.stdin.end();
      });

      const stderrText = result.stderr.trim();

      if (result.timedOut) {
        yield createFailureEvent(`codex exec timed out after ${this.timeoutMs}ms.`);
        return;
      }

      if (result.exitCode !== 0) {
        const failureMessage =
          stderrText ||
          result.stdout.trim() ||
          `codex exec exited with code ${result.exitCode ?? "unknown"}.`;
        yield createFailureEvent(failureMessage);
        return;
      }

      if (stderrText.length > 0) {
        yield { type: "warning", message: stderrText };
      }

      const lastMessage = await readFile(outputPath, "utf8").catch(() => "");
      const finalText = lastMessage.trim();

      if (finalText.length > 0) {
        yield { type: "assistant_message", text: finalText };
      }

      yield { type: "completed" };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      yield createFailureEvent(message);
    } finally {
      await rm(tempDir, { recursive: true, force: true });
    }
  }

  async interrupt(_session: RuntimeSessionRef): Promise<void> {}
}
