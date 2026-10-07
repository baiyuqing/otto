# Agent Memory 论文研究记录

访问日均为 **2026-10-07**。这些来源覆盖概念分类、事实提炼、记忆演化、生命周期治理、后台整理和评测；实验排名不作为协议依据。

## 1. Memory in the Age of AI Agents

Yuyang Hu 等；初稿 2025-12-15，核对 v2：2026-01-13；访问：2026-10-07。
[原文](https://arxiv.org/html/2512.13564v2)；关键章节：§2.2、§4.2、§5、§7.4–7.7。

摘录：“No specific internal structure is imposed”。

释义：以载体（token/参数/latent）、功能（事实/经验/工作记忆）、动态（形成/演化/召回）分别分类。形成过程可以消费交互产物、工具结果、计划和环境反馈；召回输出供模型消费的片段或结构化摘要。演化涵盖合并、冲突处理、遗忘和重组。经验记忆包含案例、策略以及可调用技能。

支持：服务不能天然等同向量数据库；输入不能限于用户事实，输出不能限于无来源字符串。
局限：分类和形式化算子不是已标准化协议；多模态、共享记忆和可信性部分包含研究展望。

## 2. Mem0: Building Production-Ready AI Agents with Scalable Long-Term Memory

Prateek Chhikara、Dev Khant、Saket Aryan、Taranjeet Singh、Deshraj Yadav；v1：2025-04-28；访问：2026-10-07。
[原文](https://arxiv.org/html/2504.19413v1)；关键章节：§2.1–2.2、§3.1、附录 B。

摘录：“candidate facts for potential inclusion in the knowledge base”。

释义：消息对结合历史摘要及近期消息，经提炼形成候选事实，再与既有记忆比较。内部选择 ADD/UPDATE/DELETE/NOOP。图版本把失效关系标记为无效而非物理删除，以保留时间推理所需信息；查询可返回相关事实或关系子图。

支持：“提交观察”与“直接写入已确认记忆”具有不同语义；接收新信息可能产生多条记忆变更。
局限：四种操作是该算法的内部决策，不应成为所有服务的必选 API。实验基于十段 LOCOMO 对话，排除了不可回答问题，不支持普适优越性或弃答能力结论。

## 3. A-Mem: Agentic Memory for LLM Agents

Wujiang Xu、Zujie Liang、Kai Mei、Hang Gao、Juntao Tan、Yongfeng Zhang；初稿 2025-02-17，核对 v11：2025-10-08；访问：2026-10-07。
[原文](https://arxiv.org/html/2502.12110v11)；关键章节：§3.1–3.4、附录 B。

摘录：“updates to the contextual representations and attributes of existing historical memories”。

释义：环境交互及时间戳形成原子笔记；笔记包含原始内容、关键词、标签、上下文描述和关联。新笔记触发关联生成，并可更新既有笔记的上下文、关键词和标签。召回先找相关笔记，再访问其关联内容。

支持：一次摄入可以演化既有对象，服务的变更结果不一定只是新 ID；原始观察与模型生成的解释应可区分。
局限：近邻搜索、链接生成是内部机制。论文中替换旧笔记的做法没有建立适合公共服务的审计、并发或回滚契约。

## 4. MemOS: A Memory OS for AI System

Zhiyu Li 等；初稿 2025-07-04，核对 v4：2025-12-03；访问：2026-10-07。
[原文](https://arxiv.org/html/2507.03724v4)；关键章节：§4.2、§5.3.2、§5.4.3、§5.5。

摘录：“metadata such as provenance and versioning”。

释义：MemCube 包装内容及元数据，描述来源、时间、语义类型、访问范围、寿命策略、版本链和使用情况。Memory API 描述创建、更新、召回、来源追踪及日志查询；生命周期允许归档、冻结、版本恢复，另有导入导出。覆盖明文、激活状态和参数记忆。

支持：来源、版本、范围、生命周期和可解释变更是独立于检索算法的服务语义；更广泛服务可能需要迁移及审计能力。
局限：这是系统架构及能力主张，不能据此承诺任意部署已实现所有事务、治理或跨类型转换；MemCube 也不是跨厂商既定格式。

## 5. LightMem: Lightweight and Efficient Memory-Augmented Generation

Jizhan Fang 等；核对 v1：2025-10-21；访问：2026-10-07。
[原文](https://arxiv.org/html/2510.18866v1)；关键章节：§3.1–3.3、§4.6。

摘录：“decouples consolidation from online inference”。

释义：先过滤压缩输入，再按主题缓存并生成摘要；记忆条目保留主题、摘要和交互内容。前台采用软追加；后台 sleep 阶段依据时间顺序整理、去重和更新。论文明确指出在线冲突判断可能误删互补信息。

支持：接收成功与整理完成可以不同；后台维护不应被强迫做成每轮同步 reflection。
局限：缓冲阈值、压缩器和更新队列属于实现选择。实验没有定义服务任务状态、整理期间可见性或取消语义，也不证明 sleep 必然提升答案质量。

## 6. LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory

Di Wu、Hongwei Wang、Wenhao Yu、Yuwei Zhang、Kai-Wei Chang、Dong Yu；初稿 2024-10-14，核对 v2：2025-03-04，ICLR 2025；访问：2026-10-07。
[原文](https://arxiv.org/html/2410.10813v2)；关键章节：§3.1、§4–5。

摘录：“information not mentioned by the user in the interaction history”。

释义：输入为带时间的多会话交互，再询问有关历史的问题。五类能力为信息提取、多会话推理、时间推理、知识更新及未知信息弃答。信息可能来自用户或助手；只存孤立事实会损失部分任务所需细节。

支持：应保留角色及时间；当前事实和历史事实需要可表达；“无匹配/证据不足”应可与成功召回区分。
局限：评测输出是助手答案，弃答不是已验证的独立召回 API；人工编写、模拟和编辑的历史不能代表全部真实工作负载。

## 7. Evaluating Memory in LLM Agents via Incremental Multi-Turn Interactions（MemoryAgentBench）

Yuanzhe Hu、Yu Wang、Julian McAuley；初稿 2025-07-07，核对 v4：2026-06-28；访问：2026-10-07。
[原文](https://arxiv.org/html/2507.05257v4)；关键章节：§3.1、§3.3、附录 B.2/B.4、K。

摘录：“without additional training”。

释义：按顺序增量输入文本，再执行任务，评估准确召回、部署时学习、长程理解和选择性遗忘。学习任务包括利用已见的带标签例子；FactConsolidation 用先后出现的矛盾事实检验当前知识更新。v1 使用 Conflict Resolution，v4 使用 Selective Forgetting。

支持：记忆输入可能是例子、反馈和规则；保留全部相关文本不等于会处理冲突。
局限：这里的遗忘主要体现为当前答案采用更新信息，不能等同物理擦除、敏感数据删除或参数 unlearning；“总是相信较晚信息”只是特定评测策略。

# 对外部能力契约的启示（研究推论）

1. 服务声明能力及各自输入输出，而非要求实现固定四个动词。可声明观察摄入、直接记忆写入、证据召回、纠正/替代、维护、删除、导出或反馈等不同能力。
2. 观察应携带稳定事件 ID、来源引用、角色、发生时间及作用范围；提炼后的记忆可携带自身 ID、版本、来源链、有效时间和派生状态。不要强制统一内部存储结构。
3. 召回优先返回可消费内容及证据引用，允许空结果或证据不足；相似度不能冒充事实可信度。数值置信度只有服务明确解释其含义时才有用。
4. 写入结果应区分已接收、已形成、已更新，以及尚待整理；后台能力可返回任务引用和状态。原始事件与记忆演化分别追踪。
5. 区分失效、替代、隐藏、归档和物理删除。基准所称 forgetting 不能替代删除契约。
6. 多模态和程序性内容可作为可选能力，以内容类型和受控资源引用扩展；检索到脚本或技能不构成执行授权。模型私有推理也不应自动成为观察输入。
7. embedding、排序器、近邻、图索引、压缩器、队列和参数转换留在服务内部；这些论文没有提供把它们规定为必需协议的依据。
