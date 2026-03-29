import { access, readFile } from "node:fs/promises";
import { join } from "node:path";

import type { ProjectContext, ProjectContextStore } from "../core/agent-kernel.js";
import type { SoulProfile, SoulStore } from "../persona/soul.js";

function createEmptySoulProfile(): SoulProfile {
  return {
    identity: [],
    temperament: [],
    standards: [],
    collaboration: [],
    voice: [],
    boundaries: [],
    updatePolicy: [],
    revisionLog: [],
  };
}

async function fileExists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

function extractBulletOrLine(line: string): string | null {
  const trimmed = line.trim();

  if (trimmed.length === 0 || trimmed.startsWith("#")) {
    return null;
  }

  if (trimmed.startsWith("- ")) {
    return trimmed.slice(2).trim();
  }

  if (trimmed.startsWith("* ")) {
    return trimmed.slice(2).trim();
  }

  return trimmed;
}

const soulSectionMap: Record<string, keyof Omit<SoulProfile, "revisionLog"> | "revisionLog"> = {
  identity: "identity",
  temperament: "temperament",
  standards: "standards",
  collaboration: "collaboration",
  voice: "voice",
  boundaries: "boundaries",
  "update policy": "updatePolicy",
  "revision log": "revisionLog",
};

function parseSoulMarkdown(markdown: string): SoulProfile {
  const profile = createEmptySoulProfile();
  let currentSection: keyof SoulProfile | null = null;

  for (const rawLine of markdown.split(/\r?\n/)) {
    const trimmed = rawLine.trim();

    if (trimmed.startsWith("## ")) {
      const name = trimmed.slice(3).trim().toLowerCase();
      currentSection = soulSectionMap[name] ?? null;
      continue;
    }

    if (!currentSection) {
      continue;
    }

    const value = extractBulletOrLine(rawLine);

    if (!value) {
      continue;
    }

    if (currentSection === "revisionLog") {
      profile.revisionLog.push({
        timestamp: "manual",
        summary: value,
        approvedBy: "human",
      });
      continue;
    }

    profile[currentSection].push(value);
  }

  return profile;
}

function parseInstructionMarkdown(markdown: string): string[] {
  return markdown
    .split(/\r?\n/)
    .map(extractBulletOrLine)
    .filter((line): line is string => line !== null);
}

export class FileSoulStore implements SoulStore {
  async load(_agentId: string, workspacePath: string): Promise<SoulProfile | null> {
    const path = join(workspacePath, "SOUL.md");

    if (!(await fileExists(path))) {
      return null;
    }

    const contents = await readFile(path, "utf8");
    return parseSoulMarkdown(contents);
  }
}

export class FileProjectContextStore implements ProjectContextStore {
  async load(_agentId: string, workspacePath: string): Promise<ProjectContext> {
    const agentMdPath = join(workspacePath, "AGENT.md");
    const agentsMdPath = join(workspacePath, "AGENTS.md");
    const sourcePath = (await fileExists(agentMdPath)) ? agentMdPath : agentsMdPath;

    if (!(await fileExists(sourcePath))) {
      return {
        instructions: [],
        files: [],
      };
    }

    const contents = await readFile(sourcePath, "utf8");

    return {
      instructions: parseInstructionMarkdown(contents),
      files: [sourcePath],
    };
  }
}
