# Letta/MemGPT and LangGraph/LangMem: Raw Records on External Memory Contracts

Scope: read-only review of official documentation, SDK source code, and papers; all records were accessed on **2026-10-07**. No files were written and no live Memory services were called. “Verified” below means evidence in a source contract or source code, not live behavior testing.

## 1. Letta Agent SDK — Memory

- URL: [Memory](https://docs.letta.com/agent-sdk/memory)
- Source date/version: The page gives no publication date or SDK version; this is current Agent SDK documentation, separate from the V1 legacy section in the navigation.
- Methods/types: `createAgent({memory, persona, human, dreaming})`, `session.getDeviceStatus()`; memory entries use `label/value`.
- Short excerpt: “Files under `system/` are in the system prompt every turn.”
- Summary: Current memory lives in a git repository owned by the agent. `system/` is always in context; other files appear in the directory tree and can be read on demand. Edits become memory after commit/push. Dreaming can trigger a background subagent based on step count or compaction, either as a reminder or automatically.
- Verified: Always-present context and on-demand access are separate channels; memory and background-management settings are external SDK options at agent creation.
- Unknown: How an external standalone Memory service can consume only Otto sessions; a unified observe operation, reflection job lookup, or capability-discovery interface. Do not treat the current SDK as the V1 blocks API.

## 2. Letta Agent SDK — Manage Shared Memory

- URL: [Manage shared memory](https://docs.letta.com/agent-sdk/repositories)
- Source date/version: No publication date or package version is listed; the page requires `backend: "cloud"`.
- Methods/types: Repository create/delete; files list/create/read/update/delete; versions list/get; agent attach/list/detach; session resources.
- Short excerpt: “Successful mutations return the resulting commit SHA.”
- Summary: A repository can be attached to multiple agents. Links to temporary resources created by the SDK are cleaned up when the session closes; persistent attachments require explicit detach. File reads support refs, updates support a content SHA256 precondition, and each change creates a commit.
- Verified: Sharing, attachment lifecycle, history reads, and optimistic concurrency control are distinct capabilities; the SDK has no restore method after repository deletion.
- Unknown: Whether deletion physically erases history/backups, tenant ACLs, cross-account sharing permissions, and multi-file transactions. Detach cannot be assumed to delete data.

## 3. Letta V1 — Archival Memory

- URL: [Archival memory](https://docs.letta.com/v1-sdk/memory/archival-memory)
- Source date/version: Date unknown; explicitly belongs to the **V1 SDK legacy**.
- Methods/types: Model tools `archival_memory_insert/search`; developer management through `client.agents.passages.*`; query/tags/page.
- Short excerpt: “Archival memory is for intentional storage”
- Summary: Archival memory is available for on-demand semantic retrieval; it cannot be pinned in context. Model-exposed insert/search tools provide fewer operations than developer-facing content management. Search over raw conversation history has a separate meaning: it automatically retains past messages, unlike long-term knowledge written through deliberate selection.
- Verified: Model tool capabilities should not equal service management capabilities; retained observations and explicit memory writes should be distinguished.
- Unknown: The page mentions passage updates, but its overview alone does not confirm the specific update endpoint; consistency, retention, feedback, and capability negotiation have not been verified.

## 4. MemGPT — Towards LLMs as Operating Systems

- URL: [arXiv v2](https://arxiv.org/html/2310.08560v2)
- Source date/version: v1: 2023-10-12; v2: **2024-02-12**.
- Methods/types: Main context, working context, FIFO, recall/archival storage; function calls and a queue manager.
- Short excerpt: “writes both the incoming message and the generated LLM output to recall storage”
- Summary: Working context remains resident; external storage must be explicitly moved into context. The queue manager stores inputs/outputs and compresses FIFO when full; the LLM modifies or retrieves memory through function calls. Paging constrains the context budget.
- Verified: This is research evidence about an internal memory hierarchy and control flow; error feedback helps the LLM adjust later calls.
- Unknown: The paper does not establish a cross-product Memory protocol; execution feedback is not equivalent to an independent user-satisfaction or retrieval-quality feedback API.

## 5. LangGraph — BaseStore

- URL: [Pinned-version SDK source](https://github.com/langchain-ai/langgraph/blob/708aaa4ebc1cba2621aa6113050eb519a5a1bfdb/libs/checkpoint/langgraph/store/base/__init__.py)
- Source date/version: Commit `708aaa4`, 2026-10-07; package release version not checked.
- Methods/types: get/put/delete/search/list_namespaces; batch/abatch and sync/async methods; `Item`, `SearchItem`.
- Short excerpt: “Natural language search support depends on your store implementation.”
- Summary: The Store provides long-term data across threads. Addresses are a namespace tuple plus key; values are JSON-serializable dictionaries. Search supports namespace prefixes, field filters, and pagination; score may be absent. Semantic search and TTL depend on the adapter; `supports_ttl` defaults to false.
- Verified: Store CRUD does not include automatic observation/summarization; support for features can vary.
- Unknown: Unified namespace authentication or shared authorization; revision/CAS, history, and transaction guarantees. A namespace is an organizational address and does not establish an authorization boundary.

## 6. LangMem — Memory Tools API

- URL: [Official tools reference](https://langchain-ai.github.io/langmem/reference/tools/)
- Source date/version: Page date and release version unknown.
- Methods/types: `create_manage_memory_tool(namespace, schema, actions_permitted, instructions, store)`; `create_search_memory_tool`.
- Short excerpt: “allows AI assistants to create, update, and delete persistent memories”
- Summary: The manage tool includes create/update/delete, fills a namespace template at runtime, and allows customization of schema and usage instructions; per-user or team-wide scope is a caller configuration choice. The search tool accesses BaseStore.
- Verified: Actions can be restricted for the model; tool names, content schemas, and allowed operations are not fixed. Direct content writes and session extraction are different operations.
- Unknown: Whether namespace configuration itself prevents unauthorized access; persistence permissions, audit, and deletion-retention semantics. Tool names should not be treated as required external RPC names.

## 7. LangMem — Extraction / MemoryStoreManager

- URL: [Pinned-version source and full docstring](https://github.com/langchain-ai/langmem/blob/48e3c11f5bb527282c7d5339c6a87a0b35abccfc/src/langmem/knowledge/extraction.py)
- Source date/version: Commit `48e3c11`, 2026-10-02; release version not checked.
- Methods/types: `create_memory_manager`, `create_memory_store_manager`; inputs messages/existing/max_steps; schemas/instructions/phases; invoke/ainvoke.
- Short excerpt: “Enriches memories stored in the configured BaseStore.”
- Summary: The stateless manager extracts results from a session and existing memories; the store manager retrieves existing items, calls the manager, then puts/deletes. Algorithms, models, and phases are configurable; automatic deletion is disabled by default.
- Verified: Session processing is a service operation with different inputs and execution cost from storage CRUD.
- Unknown: The docstring claims versioned history, but the reviewed write path updates existing namespace/key entries; no public history-read contract was found that can be relied on. This does not establish that all changes are recoverable.

## 8. LangMem — ReflectionExecutor

- URL: [Pinned-version source](https://github.com/langchain-ai/langmem/blob/48e3c11f5bb527282c7d5339c6a87a0b35abccfc/src/langmem/reflection.py)
- Source date/version: Commit `48e3c11`, 2026-10-02.
- Methods/types: Local/remote executor; `submit(payload, config, after_seconds, thread_id) -> Future`; search/asearch; shutdown.
- Short excerpt: “Handles local reflection tasks with queuing and cancellation support.”
- Summary: Local execution uses an in-memory queue, threads, and Future, and permits delayed tasks and replacement of pending tasks for the same thread. Remote execution submits a named graph through LangGraph `runs.create`, with a rollback policy; this Future resolves to None when the submission call returns.
- Verified: Reflection is an SDK execution facility that can run locally or remotely; completion of the remote Future does not mean background memory processing has completed.
- Unknown: Recovery of the local queue across processes, a unified durable job handle, exactly-once memory updates, and cancellation effects for already-running tasks.

## Implications for the Otto Interface (Research Inferences, Not an Approved Design)

- Describe available capabilities such as always-present context, retrieval, explicit content management, session observation, background enrichment/consolidation, sharing/attachment, version reads, job lookup, and feedback. Do not close the capability set around four fixed operations.
- Common boundaries can describe identity/scope, input/output schemas, whether data is modified, whether work completes synchronously or is only accepted, permission requirements, and optional features. Leave provider-specific content and algorithms to the Rust adapter/remote service.
- `observe` accepts session events without promising that each event forms a memory; `write` means direct content submission; the service declares whether background processing is exposed. Accepted, completed, searchable, and failed states must be distinct.
- Remote content returned by context/search needs provenance and trust labels; the service must not expand Otto's execution authority through returned content. Namespace, attachment, or read-only access must not be treated as a substitute for authentication or authorization.
- These sources share no Memory capability-negotiation standard. Version history, TTL, sharing, reflection, automatic deletion, semantic search, continuous observation, feedback, and durable jobs cannot be required of every service; unsupported capabilities should be explicit and must not be silently simulated as successful.
