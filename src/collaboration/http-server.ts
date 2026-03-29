import { createServer } from "node:http";
import { resolve } from "node:path";

import { SqliteCollaborationStore } from "./sqlite-store.js";

function parseArgs(argv: string[]): { databasePath: string; port: number } {
  let databasePath = ".otto/collaboration.sqlite";
  let port = 4318;

  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];
    if (!token) {
      continue;
    }

    if (token === "--port") {
      const value = argv[index + 1];
      if (!value) {
        throw new Error("Missing value after --port");
      }

      port = Number(value);
      index += 1;
      continue;
    }

    if (!token.startsWith("--")) {
      databasePath = token;
    }
  }

  return {
    databasePath: resolve(process.cwd(), databasePath),
    port,
  };
}

async function main(): Promise<void> {
  const { databasePath, port } = parseArgs(process.argv.slice(2));
  const store = new SqliteCollaborationStore(databasePath);

  const server = createServer(async (request, response) => {
    response.setHeader("Access-Control-Allow-Origin", "*");
    response.setHeader("Access-Control-Allow-Methods", "GET, OPTIONS");
    response.setHeader("Access-Control-Allow-Headers", "Content-Type");

    if (request.method === "OPTIONS") {
      response.writeHead(204);
      response.end();
      return;
    }

    if (request.method === "GET" && request.url === "/healthz") {
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ ok: true }));
      return;
    }

    if (request.method === "GET" && request.url === "/api/collaboration") {
      const snapshot = await store.readSnapshot();
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify(snapshot));
      return;
    }

    response.writeHead(404, { "Content-Type": "application/json" });
    response.end(JSON.stringify({ error: "Not found" }));
  });

  const shutdown = () => {
    server.close(() => {
      store.close();
      process.exit(0);
    });
  };

  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);

  await new Promise<void>((resolveServer) => {
    server.listen(port, "127.0.0.1", () => {
      console.log(`Collaboration API listening on http://127.0.0.1:${port}`);
      console.log(`Reading SQLite database at ${databasePath}`);
      resolveServer();
    });
  });
}

void main();
