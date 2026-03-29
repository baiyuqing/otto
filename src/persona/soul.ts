export interface SoulRevision {
  timestamp: string;
  summary: string;
  approvedBy: "human" | "policy";
}

export interface SoulProfile {
  identity: string[];
  temperament: string[];
  standards: string[];
  collaboration: string[];
  voice: string[];
  boundaries: string[];
  updatePolicy: string[];
  revisionLog: SoulRevision[];
}

export interface CandidateSoulDelta {
  summary: string;
  reason: string;
  evidence: string[];
  suggestedPatch: string;
}

export interface SoulStore {
  load(agentId: string, workspacePath: string): Promise<SoulProfile | null>;
}

function renderSection(title: string, items: string[]): string {
  if (items.length === 0) {
    return "";
  }

  return [`### ${title}`, ...items.map((item) => `- ${item}`)].join("\n");
}

export function renderSoulPrompt(profile: SoulProfile): string {
  return [
    renderSection("Identity", profile.identity),
    renderSection("Temperament", profile.temperament),
    renderSection("Standards", profile.standards),
    renderSection("Collaboration", profile.collaboration),
    renderSection("Voice", profile.voice),
    renderSection("Boundaries", profile.boundaries),
  ]
    .filter((section) => section.length > 0)
    .join("\n\n");
}

export class NullSoulStore implements SoulStore {
  async load(): Promise<SoulProfile | null> {
    return null;
  }
}
