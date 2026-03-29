import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { BuiltinSkillRegistry } from "../../src/skills/registry.js";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("BuiltinSkillRegistry", () => {
  it("auto-selects built-in skills from workspace signals", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-skills-"));
    tempDirs.push(dir);

    await writeFile(join(dir, "package.json"), '{"name":"demo"}');
    await writeFile(join(dir, "tsconfig.json"), "{}");
    await writeFile(join(dir, "AGENTS.md"), "# Rules\n- Keep modules small\n");
    await mkdir(join(dir, "tests"));
    await mkdir(join(dir, "src"));
    await writeFile(join(dir, "src", "cli.ts"), "console.log('cli');\n");

    const registry = new BuiltinSkillRegistry();
    const skills = await registry.resolve(dir);

    expect(skills.map((skill) => skill.id)).toEqual([
      "typescript-workspace",
      "cli-product",
      "project-conventions",
      "verification-discipline",
    ]);
  });
});
