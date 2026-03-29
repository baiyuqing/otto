import { resolve } from "node:path";

import { SqliteCollaborationStore } from "../collaboration/sqlite-store.js";
import { createDemoCompanySnapshot } from "./demo-company.js";

function parseArgs(argv: string[]): { databasePath: string; prompt: string } {
  let databasePath = ".otto/collaboration.sqlite";
  const promptParts: string[] = [];

  for (const token of argv) {
    if (token.endsWith(".sqlite")) {
      databasePath = token;
      continue;
    }

    promptParts.push(token);
  }

  return {
    databasePath: resolve(process.cwd(), databasePath),
    prompt:
      promptParts.join(" ").trim() ||
      "Build a new settings page, review it, and report the result like a small software company.",
  };
}

async function main(): Promise<void> {
  const { databasePath, prompt } = parseArgs(process.argv.slice(2));
  const store = new SqliteCollaborationStore(databasePath);

  try {
    const result = createDemoCompanySnapshot(prompt);
    await store.replaceSnapshot(result.snapshot);
    console.log(`Seeded company demo database at ${databasePath}`);
    console.log(
      `Company: ${result.company.name} | Agents: ${result.company.agents.map((agent) => agent.displayName).join(", ")}`,
    );
  } finally {
    store.close();
  }
}

void main();
