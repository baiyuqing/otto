export interface SkillDefinition {
  id: string;
  title: string;
  description: string;
  instructions: string[];
}

export interface WorkspaceSnapshot {
  workspacePath: string;
  hasPackageJson: boolean;
  hasTsConfig: boolean;
  hasTestsDirectory: boolean;
  hasCliEntry: boolean;
  hasAgentsFile: boolean;
  hasSoulFile: boolean;
}

export interface SkillResolver {
  resolve(workspacePath: string): Promise<SkillDefinition[]>;
}
