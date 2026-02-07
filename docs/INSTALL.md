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

Optionally verify the Codex conversation wrapper:

```bash
npm run codex:log -- --help
```

## Optional: Enable Codex Skill

If you want to trigger this workflow as a Codex skill, link the skill into your Codex skills directory:

```bash
mkdir -p "${CODEX_HOME:-$HOME/.codex}/skills"
ln -s "$(pwd)/skills/agent-trace" "${CODEX_HOME:-$HOME/.codex}/skills/agent-trace"
```

Run the command from the repository root so `$(pwd)/skills/agent-trace` points to this project.
