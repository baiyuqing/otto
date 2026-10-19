# Skill sub-agent execution: retained decision gate

Status: approved 2026-09-22. The historical implementation plan has been removed;
this note preserves the evidence boundary and unsettled validation decision,
not a claim that provider-backed measurement has completed.

## Approved execution boundary

Structured `input`/`output` frontmatter makes a skill eligible for sub-agent
execution. Eligibility is opt-in per skill, not a change to every skill's
execution mode. The skill listing marks eligible skills `exec="agent"` and
exposes their contract so the caller knows what to delegate. Inline loading
remains possible, including when composing skills. Current implementation and
listing behavior live in `crates/otto/src/skill/` and
`crates/otto/src/subagent/`.

## Evidence and unresolved validation

The motivating paper, *Subagents vs Agent Skills: Executing Reusable Knowledge
for Long-Horizon Agentic Tasks* (arXiv:2609.09233v1), compared fixed execution
modes in another harness. Its results do not establish that Otto should run
all skills as children or that model-selected execution is reliable.

The approved validation requires Otto-specific measurements of:

- peak context, distinguishing parent and child usage;
- total tokens, including delegation overhead;
- whether the model honors `exec="agent"`, observed in parent tool calls.

Provider-backed measurement is not an offline test gate. This pruning does not
establish that those measurements ran. Do not infer approval to force every
skill into a child or remove inline loading from the shipped mechanism alone.
The original decision makes observed adherence to the marking the input to any
later decision about retaining the inline escape hatch; a broader default needs
its own evidence. Git retains the original research discussion and plan.
