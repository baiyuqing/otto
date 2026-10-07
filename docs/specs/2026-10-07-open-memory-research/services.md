# Mem0、Zep/Graphiti 服务接口研究记录

访问日统一为 **2026-10-07**。仅查官方文档与官方源码；未调用真实服务。下文“已验证”指文档或代码证据，未代表线上验收。

## 1. Mem0：Add Memories

来源：[Add Memories](https://docs.mem0.ai/api-reference/memory/add-memories)。版本：Platform V3；发表日期未知。

相关符号：`POST /v3/memories/add/`、`infer`、`immutable`、`event_id`、`GET /v1/event/{event_id}/`。

短原文：“single-pass ADD-only extraction”；“where processing is synchronous”。

释义与已验证：默认 `infer=true` 将提炼与持久化放在一次调用中，提炼阶段 ADD-only；返回事件，终态为 `SUCCEEDED/FAILED`。至少指定一个 `user_id/agent_id/app_id/run_id`。支持 metadata、observation_datetime/date、timezone、timestamp。`infer=false` 存入各条消息原文，**仍是写入**。

证据差异：正文总述异步；Response schema 对 `infer=false` 的 `message/results` 明写同步。因此不能给所有路径统一同步/异步保证。`immutable` 被描述为排除后续 update/consolidation，其与所有 graph/profile 学习步骤的关系未确认。原生提炼 proposals-only、add 幂等键、CAS revision 未确认。

## 2. Mem0：Dream

来源：[Dream](https://docs.mem0.ai/platform/features/dream)。版本：当前 Platform 文档；发表日期未知。

相关符号：Synthesis、Supersede、Merge、`latest_only`、`include_merged`。

短原文：“They’re always on for every project on every plan.”

释义与已验证：Supersede/Merge 随 add 自动执行，文档称所有项目/计划始终启用；仅 Synthesis 明确可按项目开关，Pro+ 提供。Synthesis 后台写入模式记忆，保留并链接源记忆，文档明确同一证据重复运行不产生重复模式。仅独立 user scope 参与 Synthesis。Superseded 默认仍可读；merged 默认隐藏但保留。

未知：Supersede/Merge 的关闭机制、逐产物审批阶段、删除源记忆后 synthesized patterns 的撤销保证。后台生成产物不是“审核通过后才写入”的已验证流程。

## 3. Mem0：Python Platform SDK

来源：[mem0/client/main.py](https://raw.githubusercontent.com/mem0ai/mem0/main/mem0/client/main.py)。版本：`main` 快照，提交号及对应 release 未确认；发表日期未知。

相关符号：`MemoryClient.search/get/get_all/update/delete/delete_all/history/batch_update/batch_delete/generate_profile`。

短原文：“only the given memory is deleted”。

释义与已验证：search 接受 query、filters、top_k、rerank；实体 scope 放在 filters。SDK携带 org/project 参数，显式 CRUD、批量更新/删除及 history 独立存在。`delete_linked=True` 传递删除当前记忆所 supersede 的旧记忆链；默认仅删指定 ID。profile generation 有 `Idempotency-Key` 与 job 查询。

未知：不能将 profile job 幂等推广到 add；history 不证明 revision/CAS；批量原子性及 graph、profile、synthesized memory 的完整删除闭包未确认。

## 4. Zep：Create an Ingestion Pipeline

来源：[Create an Ingestion Pipeline](https://help.getzep.com/zep-ingest)。版本：Zep v3、`zep-ingest`；精确包版本及发表日期未知。

相关符号：`graph.add`、`thread.add_messages`、`Pipeline.preview/run`、`IngestResult.wait`、`search_when_ready`。

短原文：“without making any Zep API calls”。

释义与已验证：原始材料作为 episodes/messages 交给 Zep 解释；已知结构可走 nodes/fact triples。目标为 user graph 或 shared graph。`preview()` 只预览 loader、transforms、validation 的结果，**不是远端提炼候选**。返回 batch/episode/task 标识便于监控；接受的写入也可能缺少 completion handle。处理完成与搜索可见是不同检查点。重复提交产生新 episodes，包不提供内容去重或 resume checkpoint。

未知：不能把客户端输入 preview 宣称为服务 proposals-only；搜索出现任意结果不证明指定材料已正确提炼。

## 5. Zep：Manually Updating the Graph

来源：[Manually Updating the Graph](https://help.getzep.com/adding-fact-triplets)。版本：v3；发表日期未知。

相关符号：`graph.add_nodes/add_fact_triple`、`graph.node.update`、`graph.edge.update`。

短原文：“Every accepted node is a new node”。

释义与已验证：存在自动 ingestion 之外的显式写入/修改接口。add_nodes 存调用者提供的内容并产生检索表示，1–100 nodes；返回分配好的 UUID 与异步 task，成功前不可读/搜索。不能自定 node UUID；重复同内容请求产生新 nodes。按 UUID 更新节点为同步响应。

未知：显式写入不自动等于无副作用的精确 commit；并发 revision、批量原子性、普遍幂等保证未确认。

## 6. Zep：Deleting Data from the Graph

来源：[Deleting Data from the Graph](https://help.getzep.com/deleting-data-from-the-graph)。版本：v3；发表日期未知。

相关符号：`graph.edge/node/episode.delete`、`thread.delete`。

短原文：“Deleting an episode does not regenerate the names or summaries of nodes shared with other episodes.”

释义与已验证：删 edge 不删节点；删 node 级联删除连接的 edges；删 episode 只删没有其他 episode 关联的 nodes/edges。共享节点名称/summary 可能保留被删来源的信息；被该 episode invalidated 的 fact 不自动恢复。删 thread 逐 episode 采用同样规则。

未知：不能承诺删来源会清除全部派生内容；更大 scope 删除后的所有派生缓存边界未由本页确认。

## 7. Zep：Batch ingestion

来源：[Batch ingestion](https://help.getzep.com/adding-batch-data)。版本：v3；发表日期未知。

相关符号：`batch.create/add/process/get/list_items/delete`。

短原文：“Processing has not started. Can be deleted.”

释义与已验证：`draft → process → queued/processing → terminal`，draft 保存待提交输入但尚未处理，可删。单批最多 50,000 items，每次 add 最多 350；按目标 graph 分组、按添加顺序处理。有批次与逐 item 状态、partial/failed/skipped；`succeeded` 仍可能包含 skipped。metadata、source_description、document_id、created_at 记录来源和事件时间。

未知：draft 支持审核原始输入，未证明能先生成 facts proposals 再批准；状态与批量存在不证明原子性或幂等。

## 8. Graphiti：核心 SDK

来源：[graphiti_core/graphiti.py](https://raw.githubusercontent.com/getzep/graphiti/main/graphiti_core/graphiti.py)。版本：`main` 快照，提交号及对应 release 未确认；发表日期未知。

相关符号：`add_episode/add_episode_bulk/add_triplet/search/search_`、`build_communities/summarize_saga`、`get_nodes_and_edges_by_episode/remove_episode`。

短原文：“Process an episode and update the graph.”

释义与已验证：add_episode 连续执行提取、去重、失效判断、summary 更新与保存；支持 group scope、reference_time、来源描述和前序 episodes。代码分别记录 ingestion `created_at` 与事件 `valid_at`。community/saga 汇总会写图；可按 episode 查相关 nodes/edges。核心 async 函数可 await，文档建议外围队列，不能等同于内建持久 job/status 服务。remove_episode 有选择性清理，没有共享 summary 重建步骤。

未知：proposals-only、崩溃恢复、通用幂等、事务/revision及全面派生删除保证未确认。

## 对 Otto 协议的启示（研究推论）

能力面需要容纳输入 ingestion、原文/显式写入、搜索与直接读取、更新/删除、来源/时间、异步监控、后台合并与总结；不宜限定为四个固定动作，也不要求服务暴露 Otto 的 Record/Turso/FTS 仓库。

人工审核至少区分“审核输入后授权服务学习”与“审核最终变更后精确提交”。Mem0 天然适配前者；当前证据不足以证明默认原生学习适配后者。关闭 Synthesis、使用 infer=false，也不能自动证明全部后续处理停止。

协商应分别声明：服务是否会自行产生变更、能否只生成候选、能否冻结后台处理、任务与重试语义、revision/幂等保证，以及删除覆盖哪些派生物。未知能力保持未知，不能靠适配器命名将它提升为保证。
