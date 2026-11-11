# Summary-tree evaluation: acceptance rationale

Status: implemented opt-in development evaluation, approved 2026-10-27. This is
an evaluation separate from the optional live `view-tree` context strategy
described in the [manual](../manual/interactive-use.md#context-strategy). The
[testing guide](../development/testing.md#summary-tree-view-evaluation) owns
commands, artifacts, cost drivers and restart behavior. Portable tree/cost logic
lives in `otto-core::view_tree`; native evaluation execution and CLI parsing
live in `crates/otto-cli/src/eval_command.rs` and its child modules.

## Question and acceptance criteria

The experiment asks whether a summary-tree view plus selective zoom can answer
questions about an existing session with less computed input cost than the
current transcript/compaction layout. It does not establish that a live agent
should switch context strategies.

The approved acceptance criteria are:

1. View-answer correctness is not lower than baseline correctness.
2. Every wrong view answer is classified as `missing` (no node on the source
   path mentions the item), `not_zoomed` (a node mentions it but the answerer
   did not reach the source), or `other`, using the recorded zoom path and a
   model check of node text against the expected answer.
3. Computed per-turn input cost is lower in both cache-warm and cache-expired
   cases.

Build nodes, generated/validated probes, view and baseline answers, judge
reasons and costs remain separate artifacts so a person can inspect the result.
Reruns reuse prior artifacts; a report is evidence for that input, not a claim
of general acceptance. The evaluation `zoom` and `date` tools operate only on
evaluation artifacts. Live sessions register their own session-bound `zoom`;
`date` remains evaluation-only.

## Interpretation limits

- Probe generation, validation and judging are model-based, not human ground
  truth. Preserve dropped-probe reasons and answer/judge evidence for inspection.
- Cache cost is computed from byte sizes and configurable multipliers, not a
  measured provider bill. Cache granularity, retention and prices can differ.
- Replay assumes parents are built when needed; it ignores live build latency.
- Sub-agent transcripts and image content are outside this evaluation.
- Reading an existing session without its writer lease does not authorize
  rewriting it. Provider calls and written artifacts retain the normal secret
  redaction and PII egress boundaries.
