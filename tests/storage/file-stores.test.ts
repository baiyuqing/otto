import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { FileProjectContextStore, FileSoulStore } from "../../src/storage/file-stores.js";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("file stores", () => {
  it("loads SOUL.md sections into a SoulProfile", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-soul-"));
    tempDirs.push(dir);

    await writeFile(
      join(dir, "SOUL.md"),
      [
        "# Soul",
        "",
        "## Identity",
        "- Pragmatic development partner",
        "",
        "## Standards",
        "- Verify important changes",
      ].join("\n"),
    );

    const store = new FileSoulStore();
    const profile = await store.load("otto", dir);

    expect(profile?.identity).toEqual(["Pragmatic development partner"]);
    expect(profile?.standards).toEqual(["Verify important changes"]);
  });

  it("loads AGENTS.md into project instructions", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-project-"));
    tempDirs.push(dir);

    await writeFile(
      join(dir, "AGENTS.md"),
      [
        "# Repo Rules",
        "",
        "- Keep modules small",
        "- Add tests with behavior changes",
      ].join("\n"),
    );

    const store = new FileProjectContextStore();
    const context = await store.load("otto", dir);

    expect(context.instructions).toEqual([
      "Keep modules small",
      "Add tests with behavior changes",
    ]);
    expect(context.files[0]).toContain("AGENTS.md");
  });
});
