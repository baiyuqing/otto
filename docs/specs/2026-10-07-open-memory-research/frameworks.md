# Letta/MemGPT 与 LangGraph/LangMem：Memory 外部契约原始记录

范围：只读官方文档、SDK 源码与论文；访问日均为 **2026-10-07**。未写文件、未调用真实 Memory 服务。下面“已验证”指来源契约或源码，未做线上行为测试。

## 1. Letta Agent SDK — Memory

- URL：[Memory](https://docs.letta.com/agent-sdk/memory)
- 源日期/版本：页面未标发布日期和 SDK 版本；当前 Agent SDK 文档，与导航中 V1 legacy 分开。
- 方法/类型：`createAgent({memory, persona, human, dreaming})`、`session.getDeviceStatus()`；memory entry 为 `label/value`。
- 短原文：“Files under `system/` are in the system prompt every turn.”
- 释义：当前记忆在 agent 所有的 git repository 中；`system/` 是每轮常驻上下文，其余文件显示目录树，按需读取。编辑需 commit/push 才成为记忆。Dreaming 通过步骤数量或 compaction 触发后台 subagent，可提醒或自动启动。
- 已验证：常驻与按需访问是独立通道；创建时设置记忆和后台管理配置是对外 SDK 操作。
- 未确认：外部独立 Memory 服务如何只消费 Otto 会话；统一 observe、reflection job 查询或能力发现接口。不能把当前 SDK 当成 V1 blocks API。

## 2. Letta Agent SDK — Manage shared memory

- URL：[Manage shared memory](https://docs.letta.com/agent-sdk/repositories)
- 源日期/版本：未标发布日期/包版本；页面要求 `backend: "cloud"`。
- 方法/类型：repository create/delete；files list/create/read/update/delete；versions list/get；agent attach/list/detach；session resources。
- 短原文：“Successful mutations return the resulting commit SHA.”
- 释义：同一 repository 可挂给多个 agent。临时 resources 的 SDK 创建链接随 session 关闭清理；持久 attachment 要显式 detach。文件读支持 ref，更新支持内容 SHA256 前置条件，每次修改形成 commit。
- 已验证：共享、挂载生命周期、历史读取、乐观并发控制是独立能力；repository 删除后 SDK 无 restore 方法。
- 未确认：删除是否物理擦除历史/备份，tenant ACL、跨账户共享权限，以及多文件事务。detach 不能推定等于删除数据。

## 3. Letta V1 — Archival memory

- URL：[Archival memory](https://docs.letta.com/v1-sdk/memory/archival-memory)
- 源日期/版本：日期未知；明确属于 **V1 SDK legacy**。
- 方法/类型：模型工具 `archival_memory_insert/search`；开发者 `client.agents.passages.*` 管理；query/tags/page。
- 短原文：“Archival memory is for intentional storage”
- 释义：archival 是按需语义检索；不能固定在上下文。模型暴露的 insert/search 少于开发者可用的内容管理操作。原始对话搜索另有语义：自动保留过去消息，与主动筛选写入的长期知识不同。
- 已验证：模型工具能力与服务管理能力不应相等；保留观察记录和显式记忆写入应区别处理。
- 未确认：该页提到 passages update，但不能仅凭概述确认具体更新端点；未验证 consistency、retention、反馈和能力协商。

## 4. MemGPT — Towards LLMs as Operating Systems

- URL：[arXiv v2](https://arxiv.org/html/2310.08560v2)
- 源日期/版本：v1 2023-10-12；v2 **2024-02-12**。
- 方法/类型：main context、working context、FIFO、recall/archival storage；函数调用与 queue manager。
- 短原文：“writes both the incoming message and the generated LLM output to recall storage”
- 释义：工作上下文常驻；外部存储需显式搬入上下文。queue manager 保存输入/输出，溢出时压缩 FIFO；LLM 经函数修改或检索记忆。分页用于约束上下文预算。
- 已验证：这是内部 memory hierarchy 与控制流机制的研究证据；错误反馈帮助 LLM 调整后续调用。
- 未确认：论文没有建立跨产品 Memory 协议；这种执行反馈不等于独立的用户满意度或检索质量 feedback API。

## 5. LangGraph — BaseStore

- URL：[固定版本 SDK 源码](https://github.com/langchain-ai/langgraph/blob/708aaa4ebc1cba2621aa6113050eb519a5a1bfdb/libs/checkpoint/langgraph/store/base/__init__.py)
- 源日期/版本：commit `708aaa4`，2026-10-07；包发行版本未核对。
- 方法/类型：get/put/delete/search/list_namespaces；batch/abatch 与同步/异步方法；`Item`、`SearchItem`。
- 短原文：“Natural language search support depends on your store implementation.”
- 释义：Store 提供跨 thread 的长期数据。地址是 namespace tuple 加 key；value 是 JSON 可序列化字典。搜索可按 namespace 前缀、字段过滤、分页；score 可为空。语义搜索和 TTL 依赖 adapter，`supports_ttl` 默认 false。
- 已验证：存储 CRUD 不包含自动观察/总结；特性可以非全体支持。
- 未确认：统一 namespace 认证或共享授权；revision/CAS、历史版本、事务保证。namespace 是组织地址，不能从此接口推定授权边界。

## 6. LangMem — Memory Tools API

- URL：[官方工具参考](https://langchain-ai.github.io/langmem/reference/tools/)
- 源日期/版本：页面日期/发行版本未知。
- 方法/类型：`create_manage_memory_tool(namespace, schema, actions_permitted, instructions, store)`；`create_search_memory_tool`。
- 短原文：“allows AI assistants to create, update, and delete persistent memories”
- 释义：manage 工具包含 create/update/delete，运行时填写 namespace 模板，schema 与使用指令可定制；per-user 或 team-wide scope 是调用者的配置选择。search 工具访问 BaseStore。
- 已验证：可给模型限制 actions；工具的名称、内容 schema 和允许操作并非固定。直接内容写入与会话抽取不是同一操作。
- 未确认：namespace 配置本身的防越权措施；持久化权限、审计和删除保留语义。不能把工具名称当成必须采用的外部 RPC 名称。

## 7. LangMem — Extraction / MemoryStoreManager

- URL：[固定版本源码与完整 docstring](https://github.com/langchain-ai/langmem/blob/48e3c11f5bb527282c7d5339c6a87a0b35abccfc/src/langmem/knowledge/extraction.py)
- 源日期/版本：commit `48e3c11`，2026-10-02；发行版本未核对。
- 方法/类型：`create_memory_manager`、`create_memory_store_manager`；输入 messages/existing/max_steps；schemas/instructions/phases；invoke/ainvoke。
- 短原文：“Enriches memories stored in the configured BaseStore.”
- 释义：stateless manager 从会话与既有记忆抽取结果；store manager 检索既有项、调用管理器，再 put/delete。算法、模型和阶段均可配置；自动删除默认关闭。
- 已验证：会话处理是服务操作，与存储 CRUD 有不同输入和执行成本。
- 未确认：docstring 宣称 versioned history，但已查写入路径按既有 namespace/key 更新，未找到可承诺的公开历史读取契约。不能据此保证全部变化可恢复。

## 8. LangMem — ReflectionExecutor

- URL：[固定版本源码](https://github.com/langchain-ai/langmem/blob/48e3c11f5bb527282c7d5339c6a87a0b35abccfc/src/langmem/reflection.py)
- 源日期/版本：commit `48e3c11`，2026-10-02。
- 方法/类型：local/remote executor；`submit(payload, config, after_seconds, thread_id) -> Future`；search/asearch；shutdown。
- 短原文：“Handles local reflection tasks with queuing and cancellation support.”
- 释义：local 用内存队列、线程和 Future，允许延迟及同 thread 替换待执行任务。remote 通过 LangGraph `runs.create` 提交命名 graph，配置 rollback 策略；此 Future 在提交调用返回后得到 None。
- 已验证：reflection 是 SDK 执行设施，可运行本地或远端；remote Future 完成不表示后台记忆处理完成。
- 未确认：本地队列跨进程恢复、统一 durable job handle、记忆更新 exactly-once，以及已运行任务撤销效果。

## 对 Otto 接口的启示（推论，尚非批准设计）

- 将可提供的能力描述出来：常驻 context、检索、显式内容管理、观察会话、后台 enrich/consolidate、共享/挂载、版本读取、job 查询、feedback。不要以四个固定动词封闭能力集。
- 共通边界可描述身份/scope、输入输出 schema、是否修改数据、同步完成或仅受理、权限要求和可选功能；provider-specific 内容与算法留给 Rust adapter/远端服务。
- `observe` 接受会话事件，不承诺每条形成记忆；`write` 表示直接提交内容；后台 processing 是否对外开放由服务声明。已受理、完成、可检索、失败要明确区别。
- context/search 返回的远端内容需要来源与信任标记；服务不得借返回内容扩大 Otto 的执行权限。namespace、attach 或 read-only 不应被代替为认证授权保证。
- 这些来源没有共同的 Memory 能力协商标准。版本历史、TTL、共享、reflection、自动删除、语义搜索、持续观察、反馈和持久 job 都不能强制每个服务实现；unsupported 应明示，不能静默模拟成功。
