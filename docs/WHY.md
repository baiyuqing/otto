# Why Agent Trace Toolkit

## Problem
Modern AI coding workflows mix natural-language intent (LLM chat) with fast, iterative code edits. After a few iterations, it’s hard to answer:
- What changed in code, and why?
- Which conversation messages justify specific edits?
- Can we audit or replay the reasoning across files?

## Solution
Agent Trace Toolkit anchors every edit to its conversational intent and the exact AST scope. It stays offline- and Git-friendly:
- Ingests any JSONL chat (ChatGPT/Codex/Claude); optional Codex skill wrapper provided.
- Emits append-only Markdown + JSON with schema versioning, conversation windows, git head/branch/dirty flags, and optional model/latency/token metrics.
- Extracts tree-sitter AST nodes around the changed lines, storing grammar version and confidence.
- Renders static SVG plus interactive HTML with filters/search/drill-down; demo assets ship with the repo.

## AI Coding Use Cases
- Code reviews with LLM rationale: reviewers see which prompt produced which diff.
- Incident audits: trace a hotfix back to the AI instruction that triggered it.
- Pairing logs: capture “AI pair programming” sessions for knowledge sharing.
- Compliance: keep a minimal audit log of AI-assisted changes without vendor lock-in.

## Workflow Fit
- Inputs: `conversation.jsonl` (LLM chat), workspace file saves.
- Outputs: `agent-trace.md` (Markdown log), `agent-trace.json` (normalized data), `agent-trace.svg/html` (visuals).
- Tooling: Node.js 18+, tree-sitter (best-effort AST with recorded grammar version), Playwright for optional UI checks.

## Extensibility
- Language mapping lives in `scripts/trace_watch.ts`.
- UI is plain HTML/JS (`scripts/trace_ui.ts`); easy to theme or embed.
- Demo generator `npm run trace:demo-ui` lets you preview without wiring real logs.

## Related Keywords
AI coding • LLM • agent observability • code traceability • AST • diff • audit trail • Git • developer tools • ChatGPT • Codex • Claude • code review • CI/CD
