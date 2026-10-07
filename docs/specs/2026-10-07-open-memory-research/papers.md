# Research Notes on Agent Memory Papers

Access date: **2026-10-07**. These sources cover conceptual taxonomies, fact extraction, memory evolution, lifecycle governance, background consolidation, and evaluation. Experimental rankings are not treated as a basis for protocol design.

## 1. Memory in the Age of AI Agents

Yuyang Hu et al.; initial version 2025-12-15, checked against v2: 2026-01-13; accessed: 2026-10-07.
[Paper](https://arxiv.org/html/2512.13564v2); key sections: §2.2, §4.2, §5, §7.4–7.7.

Excerpt: “No specific internal structure is imposed”.

Summary: Classifies memory by substrate (tokens/parameters/latent state), function (facts/experience/working memory), and dynamics (formation/evolution/recall). Formation may consume interaction artifacts, tool results, plans, and environmental feedback; recall returns snippets or structured summaries for model consumption. Evolution covers merging, conflict resolution, forgetting, and reorganization. Experience memory includes cases, strategies, and callable skills.

Supports: A service is not inherently equivalent to a vector database; inputs need not be limited to user facts, and outputs need not be source-free strings.
Limitations: The taxonomy and formal operators are not standardized protocols; sections on multimodal and shared memory and trustworthiness include research outlook.

## 2. Mem0: Building Production-Ready AI Agents with Scalable Long-Term Memory

Prateek Chhikara, Dev Khant, Saket Aryan, Taranjeet Singh, Deshraj Yadav; v1: 2025-04-28; accessed: 2026-10-07.
[Paper](https://arxiv.org/html/2504.19413v1); key sections: §2.1–2.2, §3.1, Appendix B.

Excerpt: “candidate facts for potential inclusion in the knowledge base”.

Summary: Message pairs are combined with historical summaries and recent messages to extract candidate facts, which are then compared with existing memories. Internal choices are ADD/UPDATE/DELETE/NOOP. In the graph version, invalidated relations are marked invalid rather than physically deleted, preserving information needed for temporal reasoning; queries can return relevant facts or a relation subgraph.

Supports: “Submit an observation” and “write directly to confirmed memory” have different semantics; new information may produce multiple memory changes.
Limitations: The four operations are internal algorithmic decisions and should not be mandatory API operations for every service. Experiments use ten LOCOMO conversations and exclude unanswerable questions; they do not support claims of universal superiority or abstention capability.

## 3. A-Mem: Agentic Memory for LLM Agents

Wujiang Xu, Zujie Liang, Kai Mei, Hang Gao, Juntao Tan, Yongfeng Zhang; initial version 2025-02-17, checked against v11: 2025-10-08; accessed: 2026-10-07.
[Paper](https://arxiv.org/html/2502.12110v11); key sections: §3.1–3.4, Appendix B.

Excerpt: “updates to the contextual representations and attributes of existing historical memories”.

Summary: Environment interactions and timestamps produce atomic notes. A note contains raw content, keywords, tags, a contextual description, and links. A new note triggers link generation and may update the context, keywords, and tags of existing notes. Recall first finds relevant notes, then accesses their linked content.

Supports: Ingestion may evolve existing objects, so a service's change result is not necessarily just a new ID; raw observations and model-generated interpretations should be distinguishable.
Limitations: Nearest-neighbor search and link generation are internal mechanisms. The paper's replacement of old notes does not establish audit, concurrency, or rollback contracts suitable for a public service.

## 4. MemOS: A Memory OS for AI System

Zhiyu Li et al.; initial version 2025-07-04, checked against v4: 2025-12-03; accessed: 2026-10-07.
[Paper](https://arxiv.org/html/2507.03724v4); key sections: §4.2, §5.3.2, §5.4.3, §5.5.

Excerpt: “metadata such as provenance and versioning”.

Summary: MemCube wraps content and metadata describing provenance, time, semantic type, access scope, retention policy, version chain, and usage. The Memory API describes creation, update, recall, provenance tracing, and log queries; the lifecycle supports archiving, freezing, and version restoration, alongside import and export. It covers plaintext, activation, and parametric memory.

Supports: Provenance, versioning, scope, lifecycle, and explainable changes are service semantics independent of retrieval algorithms; broader services may need migration and audit capabilities.
Limitations: This is an architecture and capability claim; it does not justify promising that any deployment implements all transactions, governance, or cross-type conversion. MemCube is not an established cross-vendor format.

## 5. LightMem: Lightweight and Efficient Memory-Augmented Generation

Jizhan Fang et al.; checked against v1: 2025-10-21; accessed: 2026-10-07.
[Paper](https://arxiv.org/html/2510.18866v1); key sections: §3.1–3.3, §4.6.

Excerpt: “decouples consolidation from online inference”.

Summary: The system first filters and compresses input, then caches it by topic and generates summaries; memory entries retain the topic, summary, and interaction content. The foreground path uses soft appends; a background sleep phase organizes entries chronologically, deduplicates, and updates them. The paper explicitly notes that online conflict checks can incorrectly delete complementary information.

Supports: Successful receipt and completed consolidation can be separate states; background maintenance need not be forced into synchronous per-turn reflection.
Limitations: Buffer thresholds, compressors, and update queues are implementation choices. The experiments do not define service task states, visibility during consolidation, or cancellation semantics, and do not prove that sleep necessarily improves answer quality.

## 6. LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory

Di Wu, Hongwei Wang, Wenhao Yu, Yuwei Zhang, Kai-Wei Chang, Dong Yu; initial version 2024-10-14, checked against v2: 2025-03-04, ICLR 2025; accessed: 2026-10-07.
[Paper](https://arxiv.org/html/2410.10813v2); key sections: §3.1, §4–5.

Excerpt: “information not mentioned by the user in the interaction history”.

Summary: The input consists of timestamped, multi-session interactions, followed by questions about that history. The five capability categories are information extraction, multi-session reasoning, temporal reasoning, knowledge updates, and abstention when information is unknown. Information may come from either the user or the assistant; storing only isolated facts loses details needed by some tasks.

Supports: Roles and timestamps should be preserved; current and historical facts need to be representable; “no match/insufficient evidence” should be distinguishable from successful recall.
Limitations: The benchmark evaluates assistant answers; abstention is not a separately validated recall API. Human-written, simulated, and edited histories do not represent every real-world workload.

## 7. Evaluating Memory in LLM Agents via Incremental Multi-Turn Interactions (MemoryAgentBench)

Yuanzhe Hu, Yu Wang, Julian McAuley; initial version 2025-07-07, checked against v4: 2026-06-28; accessed: 2026-10-07.
[Paper](https://arxiv.org/html/2507.05257v4); key sections: §3.1, §3.3, Appendices B.2/B.4, K.

Excerpt: “without additional training”.

Summary: Text is supplied incrementally in sequence before tasks are performed, evaluating accurate retrieval, learning at deployment time, long-range understanding, and selective forgetting. Learning tasks include using previously seen labeled examples; FactConsolidation tests current knowledge updates with contradictory facts that appear at different times. v1 used Conflict Resolution; v4 uses Selective Forgetting.

Supports: Memory inputs may be examples, feedback, and rules; retaining all relevant text does not mean conflicts are handled.
Limitations: Forgetting here is primarily reflected by using updated information in the current answer; it is not equivalent to physical erasure, sensitive-data deletion, or parameter unlearning. “Always trust later information” is only a strategy for a specific evaluation.

## Implications for an External Capability Contract (Research Inferences)

1. A service should declare capabilities and their respective inputs and outputs, rather than require a fixed set of four verbs. It may declare distinct capabilities such as observation ingestion, direct memory writes, evidence retrieval, correction/replacement, maintenance, deletion, export, or feedback.
2. Observations should carry stable event IDs, source references, roles, occurrence times, and scope. Derived memories may carry their own IDs, versions, provenance chains, validity times, and derivation status. Do not require a uniform internal storage structure.
3. Recall should prioritize consumable content and evidence references, and allow empty results or insufficient evidence; similarity must not stand in for factual reliability. A numeric confidence is useful only when the service clearly explains what it means.
4. Write results should distinguish received, formed, updated, and pending consolidation states; background capabilities may return task references and status. Track raw events and memory evolution separately.
5. Distinguish invalidation, replacement, hiding, archiving, and physical deletion. Benchmark uses of “forgetting” cannot replace a deletion contract.
6. Multimodal and procedural content may be optional capabilities, extended through content types and controlled resource references; retrieving a script or skill does not grant execution authorization. Private model reasoning should not automatically become observation input.
7. Embeddings, rankers, nearest-neighbor search, graph indexes, compressors, queues, and parameter conversion should remain internal to the service; these papers provide no basis for making them required protocol elements.
