import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const exporter = fileURLToPath(new URL("./export-legacy-storage.py", import.meta.url));
const hostileText = "'\"; DROP TABLE usage_events; -- /* 雪 */ λ\0tail";

async function withDatabase(tables, check) {
  const directory = await mkdtemp(path.join(os.tmpdir(), "otto-legacy-export-"));
  try {
    const database = path.join(directory, "legacy.db");
    const created = spawnSync("python3", ["-c", `
import json, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as connection:
    for name in json.loads(sys.argv[2]):
        connection.execute('CREATE TABLE "' + name + '" (value TEXT)')
        connection.execute('INSERT INTO "' + name + '" VALUES (?)', (json.loads(sys.argv[3]),))
`, database, JSON.stringify(tables), JSON.stringify(hostileText)], { encoding: "utf8" });
    assert.equal(created.status, 0, created.stderr || String(created.error));
    const before = await readFile(database);
    const result = spawnSync("python3", [exporter, database], { encoding: "utf8" });
    assert.deepEqual(await readFile(database), before, "export must not mutate the source database");
    check(result);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("rejects dedicated retrospective and generated-skill stores without exporting rows", async () => {
  for (const tables of [
    ["runs", "watermarks"],
    ["runs", "watermarks", "generated_skills", "skill_versions"],
  ]) {
    await withDatabase(tables, (result) => {
      assert.equal(result.status, 1, result.stderr || String(result.error));
      assert.equal(result.stderr, "legacy database export failed\n");
      assert.equal(result.stdout, "", "removed stores must not produce migration records");
    });
  }
});

test("continues to export ordinary usage stores read-only", async () => {
  await withDatabase(["usage_events"], (result) => {
    assert.equal(result.status, 0, result.stderr || String(result.error));
    const records = result.stdout.trim().split("\n").map((line) => JSON.parse(line));
    assert.equal(records[0].family, "usage");
    assert.deepEqual(records[0].counts, { usage_events: 1 });
    assert.equal(records[1].table, "usage_events");
    assert.deepEqual(records[1].values, [{ type: "text", value: hostileText }]);
    assert.ok(records[0].schema.every((sql) => typeof sql === "string"), "schema is comparison metadata");
    assert.equal(records.length, 2);
    assert.equal(records[1].type, "row");
  });
});
