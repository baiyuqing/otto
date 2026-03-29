import { resolve } from "node:path";

import { createDemoCollaborationSnapshot } from "./demo-data.js";
import { SqliteCollaborationStore } from "./sqlite-store.js";

async function main(): Promise<void> {
  const targetPath = process.argv[2] ?? ".otto/collaboration.sqlite";
  const databasePath = resolve(process.cwd(), targetPath);
  const store = new SqliteCollaborationStore(databasePath);

  try {
    await store.replaceSnapshot(createDemoCollaborationSnapshot());
    console.log(`Seeded collaboration demo database at ${databasePath}`);
  } finally {
    store.close();
  }
}

void main();
