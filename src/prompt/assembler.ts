import type {
  AssembledPrompt,
  PromptAssembler,
  PromptAssemblyInput,
  PromptLayer,
} from "./layers.js";

function compareLayers(left: PromptLayer, right: PromptLayer): number {
  return left.priority - right.priority || left.kind.localeCompare(right.kind);
}

function renderLayer(layer: PromptLayer): string {
  return [
    `## ${layer.kind.toUpperCase()} (${layer.source})`,
    layer.content.trim(),
  ].join("\n");
}

export class DefaultPromptAssembler implements PromptAssembler {
  async assemble(input: PromptAssemblyInput): Promise<AssembledPrompt> {
    const layers = input.layers
      .filter((layer) => layer.content.trim().length > 0)
      .sort(compareLayers);

    return {
      system: layers.map(renderLayer).join("\n\n"),
      user: input.userMessage.trim(),
      layers,
    };
  }
}
