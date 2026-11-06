# Memory retraction evaluation: acceptance rationale

Status: implemented opt-in development evaluation, delivered under issue #591
(section 10 layers 2 and 3, section 11 evaluation PR). It is not a merge gate
and sets no release threshold. The
[testing guide](../development/testing.md#memory-retraction-evaluation) owns
the command, flags, artifacts and cost drivers. The scorer of the labeled
proposal dataset lives in `crates/otto/src/memory/proposals/evaluation.rs`;
the command, the synthetic transcripts, the evaluation memory binding, the
judges and the report live in `crates/otto-cli/src/memory_eval_command.rs`
and its child modules.

## Question and acceptance criteria

The evaluation asks two questions.

1. Layer 3: after a memory record is retired, does the model's answer stop
   relying on the obsolete claim, and does that depend on how otto presents
   the retirement? Four conditions are compared on the same synthetic
   transcripts: A (the record is active and recalled), B (the record is not
   recalled and its lifecycle is unknown, so nothing is filtered or announced
   and copies in the transcript remain), C (the lifecycle reports the
   retirement, so the fixed-template `<memory_retraction .../>` notice renders
   and the replacement record is recalled where one exists), D (C, and the
   compaction summary is rebuilt before the task turn). The obsolete claim is
   placed in five kinds of copies: a prior turn's recall reference alone, an
   assistant paraphrase with a reference, a tool result, a checkpoint summary
   with `memoryRecords`, and a legacy summary without them. Results are
   reported separately for traceable copies (a reference exists) and
   untraceable copies.
2. Layer 2: given one record and evidence, does the proposal generator choose
   the labeled action, cite the supporting evidence, keep valid claims, avoid
   inverting a withdrawn claim and avoid repeating forgotten text?

The acceptance criteria of this evaluation PR are about the harness, not
about measured rates:

1. `otto eval memory-retraction --dry-run` prints the case and provider call
   estimate without credentials and makes no provider call.
2. Every condition runs through the product agent loop with the product
   rendering of memory context, notices and summary instructions; only the
   memory binding is evaluation-specific.
3. Rule results and judge verdicts are stored in separate fields of the same
   row and reported as separate counts, never merged.
4. The report states, for each metric, the count and the sample size, per
   condition and traceability class, and names the dataset versions, prompt
   versions, model, provider, thinking setting and repetitions.
5. A rerun with the same output directory makes no provider call for a row
   that exists and rebuilds the report.
6. No written artifact or provider request holds a secret the runtime
   redactor knows; forgotten record text (the marker) appears in no agent
   request under conditions B, C and D.

## Metrics

Layer 3, per condition and traceability class, over task answers unless
named otherwise: continued reliance (rule: an `obsolete_terms` entry in the
answer; judge: `relies_on_obsolete`), correction (rule: a
`checks.must_contain_any` entry; judge: `uses_replacement`; over items with a
replacement), appropriate abstention (rule: an uncertainty term; judge:
`admits_uncertainty`; over items with `uncertainty_expected`), request
leakage (the marker in an agent request), answer leakage (the marker or an
obsolete term in the answer of a `forgotten` item), unaffected utility
(control answer holds a `control.must_contain_any` entry), cross-scope
collateral (control answer holds a `control.must_not_contain` entry),
excessive abstention (control answer with an uncertainty term and no
`must_contain_any` entry), rule and judge disagreement on continued reliance.
Condition D adds the rebuild counts: compactions run, no-ops, rebuilt
summaries still holding an obsolete term.

Layer 2: the counts of `Metrics::render` (action correct, unsupported
inversion, erroneous retirement, appropriate and excessive abstention, scope
violations, claims not preserved, cited unsupporting, missed supporting,
leakage, rejected by layer 1, unavailable) and the judge's `action_reasonable`
count with its agreement with the rule's `action_correct`.

## Interpretation limits

- The items are synthetic and few (six layer-3 items, 26 labeled proposals).
  A report is evidence for that dataset and model, not a general acceptance
  verdict; select thresholds from measured baselines, not in advance.
- Rule checks are case-insensitive substring matches on terms chosen by the
  item author; an answer can state the obsolete claim in words the terms do
  not cover. The judge is a model and can err; its verdicts are recorded for
  inspection, with the reason.
- Condition B is emulated through an `Unknown` lifecycle answer, the state a
  store without lifecycle reports; it is not a product configuration switch.
- Condition D exists only for the two summary variants. A compaction that
  finds no safe prefix is recorded as a no-op and excluded from the rebuild
  counts. The rebuilt summary comes from the provider's summarizer call and
  varies between runs.
- Each case is one turn without tools on a short transcript; long sessions,
  tool loops, sub-agents and images are outside this evaluation.
- Provider usage is reported from the provider's usage fields when present;
  it is not a measured bill.
- Reading the fixtures and writing the artifacts needs no session; the
  evaluation changes no session and registers no product tool or binding.
