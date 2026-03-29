import type { CollaborationSnapshot } from "../collaboration/demo-data.js";
import type { RuntimeTarget } from "../runtime/types.js";

export type CompanyRole = "manager" | "builder" | "reviewer";

export interface CompanyAgentProfile {
  id: string;
  displayName: string;
  role: CompanyRole;
  runtimeTarget: RuntimeTarget;
  responsibilities: string[];
}

export interface CompanyDefinition {
  id: string;
  name: string;
  projectId: string;
  userId: string;
  userDisplayName: string;
  agents: CompanyAgentProfile[];
}

export interface CompanyTaskRequest {
  id: string;
  title: string;
  prompt: string;
  createdAt: string;
}

export interface CompanySimulationResult {
  company: CompanyDefinition;
  snapshot: CollaborationSnapshot;
}
