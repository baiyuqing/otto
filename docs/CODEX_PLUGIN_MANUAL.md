# Codex Plugin 操作手册

本项目作为 Codex plugin 的实现方式是一个本地 Skill：`agent-trace`。  
目标是把「对话上下文」与「文件变更」关联，并生成可视化 SVG。

## 1. 前置条件

- Node.js 18+
- 已安装并可用的 `codex` CLI
- 已完成 `codex login`

## 2. 安装项目依赖

在项目根目录执行：

```bash
npm install
```

## 3. 启用 Plugin（注册 Skill）

把 Skill 链接到 Codex 技能目录：

```bash
ln -s /path/to/repo/skills/agent-trace "${CODEX_HOME:-$HOME/.codex}/skills/agent-trace"
```

如果你使用自定义 `CODEX_HOME`，把目标路径替换为 `$CODEX_HOME/skills/agent-trace`。

## 4. 采集对话日志（conversation.jsonl）

使用项目提供的 wrapper 启动 Codex：

```bash
npm run codex:log
```

默认输出：

- `docs/conversation.jsonl`

可自定义路径：

```bash
CODEX_CONV_LOG=/absolute/path/conversation.jsonl npm run codex:log
```

## 5. 启动追踪与生成可视化

建议使用两个终端：

1. 终端 A：启动文件追踪

```bash
npm run trace:watch -- --conversation-log docs/conversation.jsonl
```

2. 终端 B：正常开发并与 Codex 交互（可用 `npm run codex:log` 启动）

3. 任意时刻生成 SVG：

```bash
npm run trace:svg -- --log docs/agent-trace.md --out docs/agent-trace.svg
```

4. 生成可交互 HTML（推荐）：

```bash
npm run trace:ui -- --log docs/agent-trace.md --out docs/agent-trace.html
```

5. 一键生成可展示 Demo：

```bash
npm run trace:demo-ui
```

## 6. 输出文件说明

- `docs/conversation.jsonl`: 对话日志（JSONL）
- `docs/agent-trace.md`: 变更追踪日志（含机器可读 JSON 块）
- `docs/agent-trace.svg`: 对话与变更双向关系图
- `docs/agent-trace.html`: 可交互关系图（筛选/搜索/节点详情）
- `docs/agent-trace.json`: 关系图规范化数据

## 7. 常见问题

- `conversation` 字段为 `null`：
  - 检查 `--conversation-log` 路径是否正确，且 JSONL 每行是合法 JSON。
- AST 节点为空：
  - 检查文件扩展名是否在 `scripts/trace_watch.ts` 的语言映射里。
- 没有追踪记录：
  - 确认 `trace:watch` 正在运行，且保存的是仓库内文件。

## 8. 回归验证

执行：

```bash
npm test
```

测试覆盖：

- `tests/trace_watch.test.mjs`
- `tests/trace_ui.test.mjs`
- `tests/trace_svg.test.mjs`
- `tests/e2e.mjs`
