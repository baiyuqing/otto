import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { CodexRuntimeAdapter } from "../../src/runtime/codex-runtime.js";
import type { ExecuteTurnInput } from "../../src/runtime/types.js";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

function createInput(workspacePath: string): ExecuteTurnInput {
  return {
    descriptor: {
      target: "codex",
      adapterId: "local:codex",
      family: "codex",
      label: "Local Codex",
      transport: "local",
      capabilities: {
        canResumeSession: false,
        canInterrupt: false,
        supportsToolStreaming: false,
        supportsStructuredOutput: false,
      },
    },
    session: {
      target: "codex",
      adapterId: "local:codex",
      runtime: "codex",
      sessionId: "session-1",
      workspacePath,
    },
    prompt: {
      system: "System instructions",
      user: "Implement the feature",
      layers: [],
    },
    metadata: {
      agentId: "otto",
      logicalSessionId: "logical-session",
      turnId: "turn-1",
      startedAt: "2026-03-29T12:00:00.000Z",
    },
  };
}

describe("CodexRuntimeAdapter", () => {
  it("runs codex exec and returns the last assistant message", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-codex-"));
    tempDirs.push(dir);

    const executablePath = join(dir, "fake-codex");
    const promptCapturePath = join(dir, "prompt.txt");
    await writeFile(
      executablePath,
      `#!/bin/sh
set -eu
output_file=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|--output-last-message)
      output_file="$2"
      shift 2
      ;;
    -C|--cd|-s|--sandbox)
      shift 2
      ;;
    --json)
      shift 1
      ;;
    exec)
      shift 1
      ;;
    *)
      shift 1
      ;;
  esac
done
cat > "${promptCapturePath}"
printf '%s\\n' '{"type":"thread.started","thread_id":"thread-123"}'
printf '%s' 'hello from codex' > "$output_file"
`,
    );
    await chmod(executablePath, 0o755);

    const adapter = new CodexRuntimeAdapter({
      executablePath,
      sandboxMode: "read-only",
    });

    const events = [];
    for await (const event of adapter.executeTurn(createInput(dir))) {
      events.push(event);
    }

    const promptCapture = await readFile(promptCapturePath, "utf8");

    expect(promptCapture).toContain("## Framework System");
    expect(promptCapture).toContain("System instructions");
    expect(promptCapture).toContain("## User Task");
    expect(promptCapture).toContain("Implement the feature");
    expect(events).toEqual([
      { type: "assistant_message", text: "hello from codex" },
      { type: "completed" },
    ]);
    expect(adapter.capabilities.canResumeSession).toBe(false);
  });

  it("returns a failed event when codex exec exits non-zero", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-codex-fail-"));
    tempDirs.push(dir);

    const executablePath = join(dir, "fake-codex");
    await writeFile(
      executablePath,
      `#!/bin/sh
set -eu
echo 'invalid_grant: bad token' 1>&2
exit 1
`,
    );
    await chmod(executablePath, 0o755);

    const adapter = new CodexRuntimeAdapter({
      executablePath,
      sandboxMode: "read-only",
    });

    const events = [];
    for await (const event of adapter.executeTurn(createInput(dir))) {
      events.push(event);
    }

    expect(events).toEqual([
      {
        type: "failed",
        error: {
          code: "codex_exec_failed",
          message: expect.stringContaining("invalid_grant: bad token"),
        },
      },
    ]);
  });

  it("fails fast when codex exec times out", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-codex-timeout-"));
    tempDirs.push(dir);

    const executablePath = join(dir, "fake-codex");
    await writeFile(
      executablePath,
      `#!/bin/sh
set -eu
sleep 2
`,
    );
    await chmod(executablePath, 0o755);

    const adapter = new CodexRuntimeAdapter({
      executablePath,
      sandboxMode: "read-only",
      timeoutMs: 50,
    });

    const events = [];
    for await (const event of adapter.executeTurn(createInput(dir))) {
      events.push(event);
    }

    expect(events).toEqual([
      {
        type: "failed",
        error: {
          code: "codex_exec_failed",
          message: expect.stringContaining("timed out"),
        },
      },
    ]);
  });
});
