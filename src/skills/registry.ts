import { access } from "node:fs/promises";
import { join } from "node:path";

import type { SkillDefinition, SkillResolver, WorkspaceSnapshot } from "./types.js";

async function exists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

async function snapshotWorkspace(workspacePath: string): Promise<WorkspaceSnapshot> {
  return {
    workspacePath,
    hasPackageJson: await exists(join(workspacePath, "package.json")),
    hasTsConfig: (await exists(join(workspacePath, "tsconfig.json"))) ||
      (await exists(join(workspacePath, "tsconfig.build.json"))),
    hasTestsDirectory: await exists(join(workspacePath, "tests")),
    hasCliEntry: await exists(join(workspacePath, "src", "cli.ts")),
    hasAgentsFile: (await exists(join(workspacePath, "AGENT.md"))) ||
      (await exists(join(workspacePath, "AGENTS.md"))),
    hasSoulFile: await exists(join(workspacePath, "SOUL.md")),
  };
}

const builtinSkillDefinitions: Array<{
  skill: SkillDefinition;
  matches(snapshot: WorkspaceSnapshot): boolean;
}> = [
  {
    skill: {
      id: "typescript-workspace",
      title: "TypeScript Workspace",
      description: "Prefer strict types, minimal surface changes, and verified scripts.",
      instructions: [
        "Preserve TypeScript type safety and prefer framework-owned domain types.",
        "When changing behavior, keep build, typecheck, and tests runnable.",
        "Treat package scripts and tsconfig as part of the public developer workflow.",
      ],
    },
    matches(snapshot) {
      return snapshot.hasPackageJson || snapshot.hasTsConfig;
    },
  },
  {
    skill: {
      id: "cli-product",
      title: "CLI Product",
      description: "Optimize for helpful defaults, clear flags, and readable errors.",
      instructions: [
        "CLI behavior should be discoverable from help text and examples.",
        "Prefer stable flags and explicit error messages over magic behavior.",
        "If the command cannot execute work, explain why and what the user should do next.",
      ],
    },
    matches(snapshot) {
      return snapshot.hasCliEntry;
    },
  },
  {
    skill: {
      id: "project-conventions",
      title: "Project Conventions",
      description: "Respect repository-local operating instructions before guessing.",
      instructions: [
        "Load repository instructions from AGENT.md or AGENTS.md before making assumptions.",
        "Treat workspace conventions as part of the prompt stack, not as optional comments.",
      ],
    },
    matches(snapshot) {
      return snapshot.hasAgentsFile;
    },
  },
  {
    skill: {
      id: "verification-discipline",
      title: "Verification Discipline",
      description: "Prefer running checks when the workspace exposes them.",
      instructions: [
        "When the repository has tests, factor verification into the normal workflow.",
        "Report which checks ran and which checks still remain.",
      ],
    },
    matches(snapshot) {
      return snapshot.hasTestsDirectory;
    },
  },
];

export class BuiltinSkillRegistry implements SkillResolver {
  async resolve(workspacePath: string): Promise<SkillDefinition[]> {
    const snapshot = await snapshotWorkspace(workspacePath);

    return builtinSkillDefinitions
      .filter((entry) => entry.matches(snapshot))
      .map((entry) => entry.skill);
  }
}
