import { resolve } from "node:path";

import type { RuntimeTarget } from "../runtime/types.js";

export interface CliOptions {
  runtimeTarget: RuntimeTarget;
  workspacePath: string;
  sessionHint?: string;
  json: boolean;
  listRuntimes: boolean;
  remoteRuntimes: string[];
  remoteServerUrl?: string;
  remoteMachineLabel?: string;
  message?: string;
}

export class CliUsageError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "CliUsageError";
  }
}

function requireValue(argv: string[], index: number, flag: string): string {
  const value = argv[index + 1];

  if (!value) {
    throw new CliUsageError(`Missing value for ${flag}.`);
  }

  return value;
}

function parseCommaSeparated(value: string): string[] {
  return value
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
}

export function formatUsage(): string {
  return [
    "Usage:",
    '  npm run dev -- --runtime demo "your prompt"',
    '  npm run dev -- --runtime remote:codex --remote-runtimes claude,codex,gemini "your prompt"',
    "  npm run dev -- --list-runtimes --remote-runtimes claude,codex,gemini",
    '  npm run cli -- --runtime demo "your prompt"',
    "",
    "Options:",
    "  --runtime <target>",
    "  --workspace <path>",
    "  --session <session-id>",
    "  --list-runtimes",
    "  --remote-runtimes <claude,codex,gemini>",
    "  --remote-server-url <url>",
    "  --remote-machine-label <label>",
    "  --json",
  ].join("\n");
}

export function parseCliArgs(argv: string[]): CliOptions {
  let runtimeTarget: RuntimeTarget = "demo";
  let workspacePath = process.cwd();
  let sessionHint: string | undefined;
  let json = false;
  let listRuntimes = false;
  let remoteServerUrl: string | undefined;
  let remoteMachineLabel: string | undefined;
  let remoteRuntimes = parseCommaSeparated(process.env.REMOTE_RUNTIMES ?? "");
  const messageParts: string[] = [];

  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];

    if (!token) {
      continue;
    }

    switch (token) {
      case "--runtime":
        runtimeTarget = requireValue(argv, index, token);
        index += 1;
        break;
      case "--workspace":
        workspacePath = resolve(requireValue(argv, index, token));
        index += 1;
        break;
      case "--session":
        sessionHint = requireValue(argv, index, token);
        index += 1;
        break;
      case "--json":
        json = true;
        break;
      case "--list-runtimes":
        listRuntimes = true;
        break;
      case "--remote-runtimes":
        remoteRuntimes = parseCommaSeparated(requireValue(argv, index, token));
        index += 1;
        break;
      case "--remote-server-url":
        remoteServerUrl = requireValue(argv, index, token);
        index += 1;
        break;
      case "--remote-machine-label":
        remoteMachineLabel = requireValue(argv, index, token);
        index += 1;
        break;
      case "--help":
      case "-h":
        throw new CliUsageError(formatUsage());
      default:
        messageParts.push(token);
        break;
    }
  }

  const message = messageParts.join(" ").trim();

  if (!listRuntimes && message.length === 0) {
    throw new CliUsageError("A prompt message is required.");
  }

  return {
    runtimeTarget,
    workspacePath,
    ...(sessionHint ? { sessionHint } : {}),
    json,
    listRuntimes,
    remoteRuntimes,
    ...(remoteServerUrl ? { remoteServerUrl } : {}),
    ...(remoteMachineLabel ? { remoteMachineLabel } : {}),
    ...(message ? { message } : {}),
  };
}
