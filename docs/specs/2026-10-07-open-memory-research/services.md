# Mem0, Zep, and Graphiti Service Interface Research Records

Accessed on **2026-10-07**. Only official documentation and official source code were consulted; no live services were called. “Verified” below means supported by documentation or code evidence, not validated against a live deployment.

## 1. Mem0: Add Memories

Source: [Add Memories](https://docs.mem0.ai/api-reference/memory/add-memories). Version: Platform V3; publication date unknown.

Relevant symbols: `POST /v3/memories/add/`, `infer`, `immutable`, `event_id`, `GET /v1/event/{event_id}/`.

Short excerpts: “single-pass ADD-only extraction”; “where processing is synchronous”.

Summary and verified facts: By default, `infer=true` performs extraction and persistence in one call, with ADD-only behavior during extraction. It returns an event whose terminal state is `SUCCEEDED/FAILED`. At least one of `user_id/agent_id/app_id/run_id` is required. Supported fields include metadata, observation_datetime/date, timezone, and timestamp. `infer=false` stores each message verbatim; **this is still a write**.

Evidence discrepancy: The general description says processing is asynchronous, while the Response schema explicitly describes `message/results` for `infer=false` as synchronous. The evidence does not establish a uniform sync/async guarantee across all paths. `immutable` is described as excluding later update/consolidation, but its interaction with all graph/profile learning steps is unconfirmed. Native proposals-only extraction, an add idempotency key, and CAS revisions are unconfirmed.

## 2. Mem0: Dream

Source: [Dream](https://docs.mem0.ai/platform/features/dream). Version: current Platform documentation; publication date unknown.

Relevant symbols: Synthesis, Supersede, Merge, `latest_only`, `include_merged`.

Short excerpt: “They’re always on for every project on every plan.”

Summary and verified facts: Supersede/Merge run automatically with add; the documentation says they are always enabled for every project and plan. Only Synthesis is explicitly configurable per project, and is available on Pro+. Synthesis writes patterns to memory in the background, retains and links their source memories, and the documentation states that rerunning on the same evidence does not create duplicate patterns. Only independent user scopes participate in Synthesis. Superseded memories remain readable by default; merged memories are hidden by default but retained.

Unknowns: How to disable Supersede/Merge, whether each output has an approval stage, and whether deleting source memories revokes synthesized patterns. It is not verified that background-generated outputs are written only after approval.

## 3. Mem0: Python Platform SDK

Source: [mem0/client/main.py](https://raw.githubusercontent.com/mem0ai/mem0/main/mem0/client/main.py). Version: `main` snapshot; commit and corresponding release unconfirmed; publication date unknown.

Relevant symbols: `MemoryClient.search/get/get_all/update/delete/delete_all/history/batch_update/batch_delete/generate_profile`.

Short excerpt: “only the given memory is deleted”.

Summary and verified facts: `search` accepts query, filters, top_k, and rerank; entity scope is placed in filters. The SDK carries org/project parameters and exposes separate CRUD, batch update/delete, and history operations. `delete_linked=True` propagates deletion to older memories superseded by the current memory; by default, only the specified ID is deleted. Profile generation has an `Idempotency-Key` and job lookup.

Unknowns: Profile-job idempotency cannot be generalized to add; history does not establish revisions/CAS; batch atomicity and complete deletion closure across graph, profile, and synthesized memory are unconfirmed.

## 4. Zep: Create an Ingestion Pipeline

Source: [Create an Ingestion Pipeline](https://help.getzep.com/zep-ingest). Version: Zep v3, `zep-ingest`; exact package version and publication date unknown.

Relevant symbols: `graph.add`, `thread.add_messages`, `Pipeline.preview/run`, `IngestResult.wait`, `search_when_ready`.

Short excerpt: “without making any Zep API calls”.

Summary and verified facts: Zep interprets raw material submitted as episodes/messages; known structures can be supplied as nodes/fact triples. The target can be a user graph or shared graph. `preview()` previews only loader, transform, and validation results; it is **not remote extraction into candidates**. Returned batch/episode/task identifiers support monitoring; an accepted write may still lack a completion handle. Processing completion and search visibility are separate checkpoints. Repeated submissions create new episodes; the package provides neither content deduplication nor a resume checkpoint.

Unknowns: Client-side input preview does not establish service-side proposals-only behavior; finding arbitrary results in search does not prove that the specified material was extracted correctly.

## 5. Zep: Manually Updating the Graph

Source: [Manually Updating the Graph](https://help.getzep.com/adding-fact-triplets). Version: v3; publication date unknown.

Relevant symbols: `graph.add_nodes/add_fact_triple`, `graph.node.update`, `graph.edge.update`.

Short excerpt: “Every accepted node is a new node”.

Summary and verified facts: Explicit write/update interfaces exist in addition to automatic ingestion. `add_nodes` stores caller-provided content and creates retrieval representations, for 1–100 nodes; it returns assigned UUIDs and an asynchronous task, and the nodes cannot be read/searched before success. Node UUIDs cannot be chosen by the caller; repeating a request with identical content creates new nodes. Updating a node by UUID returns synchronously.

Unknowns: Explicit writes do not automatically mean an exact commit without side effects; concurrent revisions, batch atomicity, and general idempotency guarantees are unconfirmed.

## 6. Zep: Deleting Data from the Graph

Source: [Deleting Data from the Graph](https://help.getzep.com/deleting-data-from-the-graph). Version: v3; publication date unknown.

Relevant symbols: `graph.edge/node/episode.delete`, `thread.delete`.

Short excerpt: “Deleting an episode does not regenerate the names or summaries of nodes shared with other episodes.”

Summary and verified facts: Deleting an edge does not delete its nodes; deleting a node cascades to connected edges; deleting an episode removes only nodes/edges that are not associated with another episode. Names/summaries of shared nodes may retain information from the deleted source; facts invalidated by that episode are not automatically restored. Deleting a thread applies the same rules to each episode.

Unknowns: Deleting a source cannot be claimed to remove all derived content; this page does not establish the boundaries of all derived caches after deleting a broader scope.

## 7. Zep: Batch Ingestion

Source: [Batch ingestion](https://help.getzep.com/adding-batch-data). Version: v3; publication date unknown.

Relevant symbols: `batch.create/add/process/get/list_items/delete`.

Short excerpt: “Processing has not started. Can be deleted.”

Summary and verified facts: `draft → process → queued/processing → terminal`; a draft stores input that has not yet been processed and can be deleted. A batch can contain up to 50,000 items, with up to 350 per add call; items are grouped by target graph and processed in add order. Batch-level and per-item states include partial/failed/skipped; `succeeded` may still include skipped items. Metadata, source_description, document_id, and created_at record provenance and event time.

Unknowns: Drafts support review of raw input, but it is not established that facts can first be produced as proposals and then approved. Batch status and batching do not establish atomicity or idempotency.

## 8. Graphiti: Core SDK

Source: [graphiti_core/graphiti.py](https://raw.githubusercontent.com/getzep/graphiti/main/graphiti_core/graphiti.py). Version: `main` snapshot; commit and corresponding release unconfirmed; publication date unknown.

Relevant symbols: `add_episode/add_episode_bulk/add_triplet/search/search_`, `build_communities/summarize_saga`, `get_nodes_and_edges_by_episode/remove_episode`.

Short excerpt: “Process an episode and update the graph.”

Summary and verified facts: `add_episode` sequentially performs extraction, deduplication, invalidation checks, summary updates, and persistence. It supports group scope, reference_time, source descriptions, and preceding episodes. The code records ingestion `created_at` separately from event `valid_at`. Community/saga summaries write to the graph; related nodes/edges can be retrieved by episode. Core async functions can be awaited, and the documentation recommends an external queue; this is not equivalent to a built-in durable job/status service. `remove_episode` performs selective cleanup and has no step to rebuild shared summaries.

Unknowns: Proposals-only behavior, crash recovery, general idempotency, transactions/revisions, and comprehensive deletion of derived content are unconfirmed.

## Implications for the Otto Contract (Research Inferences)

The capability surface needs to accommodate input ingestion, verbatim/explicit writes, search and direct reads, updates/deletion, provenance/time, asynchronous monitoring, and background merging/summarization. It should not be limited to four fixed operations, nor require a service to expose Otto's Record/Turso/FTS repository.

Human review should at least distinguish “review input, then authorize service learning” from “review final changes, then commit them exactly.” Mem0 naturally fits the former; current evidence does not establish that its default native learning fits the latter. Disabling Synthesis or using `infer=false` also does not by itself prove that all subsequent processing has stopped.

Negotiation should separately declare whether the service can create changes autonomously, produce candidates only, freeze background processing, and what task/retry semantics, revision/idempotency guarantees, and derived-data deletion coverage it provides. Unknown capabilities must remain unknown; adapter naming cannot elevate them into guarantees.
