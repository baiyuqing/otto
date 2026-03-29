# Trial Manual

## What You Can Try Today

This repository already supports a useful local trial flow for the framework itself:

- boot the CLI
- auto-detect built-in skills from the workspace
- load `AGENT.md` or `AGENTS.md`
- load `SOUL.md` if present
- list local and remote daemon runtime targets
- route a request into the kernel
- see prompt-layer and runtime-target behavior end to end

What is **not** implemented yet:

- real `Codex` execution
- real `Claude Code` execution
- real remote daemon transport
- durable memory writeback beyond the null engine

So the right way to trial this version is:

- use `demo` to validate the framework flow
- use `--list-runtimes` to validate runtime inventory
- use `remote:*` targets to validate routing and target modeling

## Prerequisites

- Node.js 20+ recommended
- `npm`

## One-Time Setup

From the repository root:

```bash
npm install
```

Optional verification:

```bash
npm run typecheck
npm test
npm run build
```

## Fastest Trial

Run the framework with the built-in demo runtime:

```bash
npm run dev -- --runtime demo "show me bootstrap skills"
```

Expected output shape:

```text
runtime: demo
session: <uuid>
skills: typescript-workspace, cli-product, project-conventions, verification-discipline

Demo runtime response.
...
Prompt layers: base, project, skill, task, runtime
```

This confirms:

- the CLI started
- the kernel executed
- built-in skills were auto-selected
- prompt layers were assembled
- the runtime adapter returned a result

## List Available Runtime Targets

To see what the framework can target right now:

```bash
npm run dev -- --list-runtimes
```

Expected output:

```text
demo  [local]  Local Demo Runtime
codex  [local]  Local Codex
claude-code  [local]  Local Claude Code
```

## Simulate Remote Runtime Discovery

The current code can model daemon-discovered remote runtimes even though it does not execute them yet.

List local plus remote daemon targets:

```bash
npm run dev -- \
  --list-runtimes \
  --remote-runtimes claude,codex,gemini \
  --remote-server-url https://runtime.example.com \
  --remote-machine-label Yuqing
```

Expected output:

```text
demo  [local]  Local Demo Runtime
codex  [local]  Local Codex
claude-code  [local]  Local Claude Code
remote:claude  [daemon]  Remote claude
remote:codex  [daemon]  Remote codex
remote:gemini  [daemon]  Remote gemini
```

This confirms that the framework now treats the remote daemon as:

- runtime inventory source
- daemon-backed transport layer
- multi-runtime host

and not as one single runtime id.

## Try Remote Routing

You can also verify that the framework routes `remote:*` correctly:

```bash
npm run dev -- \
  --runtime remote:codex \
  --remote-runtimes claude,codex,gemini \
  "hello through remote"
```

Expected result:

```text
Remote daemon transport for "remote:codex" is not implemented yet.
```

That error is expected. It proves the request was routed to the remote daemon adapter instead of the local `codex` adapter.

## Use a Specific Workspace

The kernel reads workspace files from the path you pass in.

```bash
npm run dev -- \
  --runtime demo \
  --workspace /absolute/path/to/your/repo \
  "review this workspace setup"
```

The framework will look for:

- `AGENT.md` or `AGENTS.md`
- `SOUL.md`
- workspace signals such as `package.json`, `tsconfig.json`, `tests/`, and `src/cli.ts`

## How Auto Skills Work

Built-in skills are selected from workspace structure. In the current implementation, these skills may activate:

- `typescript-workspace`
- `cli-product`
- `project-conventions`
- `verification-discipline`

They are resolved in:

- `src/skills/registry.ts`

The selected skills are injected as a dedicated `skill` prompt layer.

## Minimal SOUL.md to Try

Create a `SOUL.md` in the workspace root if you want to test identity loading:

```md
# Soul

## Identity
- Pragmatic development partner.

## Temperament
- Direct under normal conditions.

## Standards
- Prefer verifiable claims.

## Collaboration
- Act autonomously when the path is clear.

## Voice
- Concise by default.

## Boundaries
- Do not invent facts.
```

Then rerun:

```bash
npm run dev -- --runtime demo "what kind of agent are you?"
```

The current demo runtime will not print the assembled prompt, but the kernel will load the soul and include it as a prompt layer.

## Minimal AGENT.md or AGENTS.md to Try

Add one of these files in the workspace root:

```md
# Repo Rules

- Keep modules small.
- Add tests with behavior changes.
- Prefer explicit runtime targets over hidden defaults.
```

This becomes the `project` prompt layer.

## Use the Built CLI

If you prefer running compiled code:

```bash
npm run build
npm run cli -- --runtime demo "hello from built cli"
```

## JSON Output

To inspect the raw result object:

```bash
npm run dev -- --runtime demo --json "dump the result"
```

This is useful if you want to inspect:

- `logicalSessionId`
- selected runtime target
- diagnostics
- writeback summary

## Sessions

You can reuse a logical session id:

```bash
npm run dev -- --runtime demo --session my-test-session "first turn"
npm run dev -- --runtime demo --session my-test-session "second turn"
```

Current limitation:

- session continuity is in-memory only for the running process
- there is no persistent session backend yet

So this is best used as a structural trial, not as durable continuity.

## Try the 3-Agent Company Demo

If you want to see the system as a small company instead of a single thread snapshot, seed the dedicated company demo database:

```bash
npm run company:seed-demo -- .otto/company.sqlite
npm run collab:serve -- .otto/company.sqlite --port 4318
cd web && npm run dev
```

What this gives you:

- one private DM between you and the manager agent
- one public channel thread owned by the manager
- one internal agent dialogue for manager, builder, and reviewer
- one linked task with activity events such as status updates, shell work, and handoff

Default company roles:

- `Mara (Manager)` owns the public thread
- `Jules (Builder)` does the implementation slice
- `Nia (Reviewer)` checks the result and hands back sign-off

You can also override the company task prompt:

```bash
npm run company:seed-demo -- .otto/company.sqlite "Build a settings page, review it, and summarize the result publicly."
```

Open the UI and look for:

- `SQLite live data` in the top bar
- a public `# company-updates` thread
- an internal agent conversation linked to the same task
- `Recent activity` in the right sidebar

## Current Trial Checklist

Use this checklist if you want to validate the framework quickly:

1. `npm install`
2. `npm run dev -- --runtime demo "show me bootstrap skills"`
3. `npm run dev -- --list-runtimes`
4. `npm run dev -- --list-runtimes --remote-runtimes claude,codex,gemini`
5. add a test `SOUL.md`
6. add a test `AGENT.md` or `AGENTS.md`
7. rerun the demo command with `--workspace`
8. optionally run `npm run dev -- --runtime remote:codex --remote-runtimes claude,codex,gemini "hello"`
9. optionally run `npm run company:seed-demo -- .otto/company.sqlite` and inspect the small-company flow in the web UI

If all of that works, the framework shell is behaving as designed.

## Troubleshooting

### `A prompt message is required.`

You forgot the final free-form message, or you used a non-listing command without a prompt.

### `Runtime target "<x>" is not available.`

The target is not in the current runtime inventory. Use `--list-runtimes` first.

### `Remote daemon transport for "remote:codex" is not implemented yet.`

Expected for now. Discovery and routing exist; transport does not.

### No `skills:` line in output

The selected workspace does not match any built-in skill conditions, or you ran against a directory without the expected files.

## What To Test Next

Once you are comfortable with this manual, the next meaningful step is to implement one of these:

1. real `CodexRuntimeAdapter`
2. real `RemoteDaemonAdapter`
3. persistent session store
4. file-backed memory engine

Until then, this repo is best treated as a trialable framework shell rather than a production agent.
