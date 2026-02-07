declare module "tree-sitter" {
  export type Point = { row: number; column: number };
  export interface SyntaxNode {
    type: string;
    startIndex: number;
    endIndex: number;
    startPosition: Point;
    endPosition: Point;
    namedChildren: SyntaxNode[];
  }
  export interface Tree {
    rootNode: SyntaxNode;
  }
  export interface Language {}

  export default class Parser {
    setLanguage(language: Language): void;
    parse(input: string): Tree;
  }

  export namespace Parser {
    export type Language = Language;
    export type SyntaxNode = SyntaxNode;
  }
}

declare module "tree-sitter-python" {
  import { Language } from "tree-sitter";
  const language: Language;
  export default language;
}

declare module "tree-sitter-go" {
  import { Language } from "tree-sitter";
  const language: Language;
  export default language;
}

declare module "tree-sitter-javascript" {
  import { Language } from "tree-sitter";
  const language: Language;
  export default language;
}

declare module "tree-sitter-typescript" {
  import { Language } from "tree-sitter";
  const languages: { typescript: Language; tsx: Language };
  export default languages;
}
