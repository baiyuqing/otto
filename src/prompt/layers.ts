export type PromptLayerKind =
  | "base"
  | "soul"
  | "project"
  | "skill"
  | "task"
  | "memory"
  | "reflection"
  | "runtime";

export interface PromptLayer {
  kind: PromptLayerKind;
  priority: number;
  content: string;
  source: string;
}

export interface PromptAssemblyInput {
  userMessage: string;
  layers: PromptLayer[];
}

export interface AssembledPrompt {
  system: string;
  user: string;
  layers: PromptLayer[];
}

export interface PromptAssembler {
  assemble(input: PromptAssemblyInput): Promise<AssembledPrompt>;
}
