# Open Memory contract research

Status: source research completed on 2026-10-07; not an implementation claim.

The original records below were collected independently before the contract
was designed. They retain source dates or versions, short excerpts, verified
facts, unknown guarantees, and the researchers' separately labelled inferences.
No live memory service was called. Unknown capabilities are not treated as
unsupported capabilities, and documentation evidence is not runtime acceptance.
Statements such as "未写文件" describe the original Plan-mode research phase;
the records have since been archived here without changing their findings.

| Record | Sources | Coverage |
| --- | --- | --- |
| [Services](services.md) | 8 | Mem0 Platform V3, Dream, Zep v3, Graphiti |
| [Frameworks](frameworks.md) | 8 | Letta current and legacy APIs, MemGPT, LangGraph, LangMem |
| [Papers](papers.md) | 7 | Formation, evolution, retrieval, lifecycle and evaluation |

The [behavior contract](../2026-10-07-open-memory-contract.md) is the synthesis;
it does not turn every feature described by a paper into a required interface.
Floating documentation and `main` snapshots are explicitly identified in the
records. Their guarantees must be checked again against a selected adapter's
actual deployment/version before implementation acceptance.

## Additional source: memory retrieval as a trust boundary

- Title: *Beyond Similarity: Trustworthy Memory Search for Personal AI Agents*.
- Authors: Jiawen Zhang et al.
- Version/date: arXiv 2606.06054 v1, 2026-06-04.
- Accessed: 2026-10-07.
- Source: <https://arxiv.org/abs/2606.06054v1>.
- Relevant passage: abstract, especially the distinction between semantic
  relevance and contextually appropriate memory admission.
- Short excerpt: "a semantically related memory may still be contextually inappropriate".
- Verified source claim: the paper studies memory search as a trust boundary
  and evaluates a query-conditioned admission mechanism in several agent
  memory settings.
- Limitation: this record is based on the abstract, not a reproduced experiment
  or an independent evaluation of its proposed model.
- Contract inference: recall needs task context and provenance, and Otto must
  retain authority over scope, prompt admission and execution permissions.
  Similarity scores are not approval or trust evidence. The paper does not
  justify requiring its particular gate/model in the Memory interface.
