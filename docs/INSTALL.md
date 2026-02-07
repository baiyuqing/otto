# Installation

This guide explains how to install the Agent Trace Toolkit locally.

## Requirements

- Node.js 18+
- A writable workspace

## Install Dependencies

```bash
npm install
```

## Verify

Run the watcher once to confirm dependencies are available:

```bash
npm run trace:watch -- --help
```

If the help text prints, the installation is complete.

## Optional: Enable Codex Skill

If you want to trigger this workflow as a Codex skill, link the skill into your Codex skills directory:

```bash
ln -s /Users/baiyuqing/Work/code/ai/otto/skills/agent-trace /Users/baiyuqing/.codex/skills/agent-trace
```

Adjust the paths if your workspace or Codex home directory differs.
