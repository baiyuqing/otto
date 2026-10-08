<p align="center">
  <img src="docs/logo.svg?v=2" alt="Otto 标志" width="320">
</p>

# Otto — 本地优先的通用个人 AI Agent

[English](README.md) · [用户手册（英文）](docs/user-manual.md) · [更新日志（英文）](CHANGELOG.md)

**Otto 是一个使用 Rust 编写的通用个人 AI Agent，帮助你研究、写作、规划、编程和处理日常任务。**
它按需调用当前会话可用的工具，并受对应权限约束。可在终端和浏览器中使用，
也可通过 `otto-connect` 从 Telegram 或飞书使用；连接 `otto serve` 时，这些聊天还能共享服务端会话。
支持通过 API key 连接 OpenAI-compatible 接口，或通过 `otto login` 登录使用 ChatGPT provider。
模型请求会发送给所选服务；“本地优先”指运行时、会话历史和默认记忆存储位于本地，并不表示模型在本地运行。

- **多种入口：** 终端中的全屏 TUI、REPL 和脚本模式，独立的 [otto-plus](https://github.com/baiyuqing/otto-plus) Web UI，以及通过 `otto-connect` 接入的 Telegram、飞书。
- **持续协作：** 持久化会话、统一审批的记忆候选、可复用 Skills、按需或自动反思、有边界的子代理、定时器和可恢复的持久化 workflow。
- **限定访问范围：** 文件工具限制在工作区内；macOS 默认通过 Seatbelt 沙箱执行 Shell 命令。

## 代码架构

[![Otto 代码架构：接入端、前端适配、应用运行时、原生功能服务与可移植核心](docs/architecture/otto-architecture.png?v=a37f68b)](docs/architecture/otto-architecture.svg)

[代码组织（英文）](docs/code-organization.md) · [SVG 矢量图](docs/architecture/otto-architecture.svg) · [可编辑 HTML](docs/architecture/otto-architecture.html)

## 从源码安装

需要由 `rust-toolchain.toml` 锁定的 **Rust 1.98** 工具链
（`rustup toolchain install` 会自动安装）、`chat-connector/go.mod` 指定的 **Go**，
以及受支持服务的访问权限。**macOS** 是受支持的平台：只有它有沙箱，
也只有它跑验收门禁。Otto 同样可以在 **Linux** 上编译和运行，限制见下方
[安全与限制](#安全与限制)。

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make install
otto --help
```

`make install` 构建 Otto 和 `otto-connect`，将两者安装到 `~/.local/bin/`，
并将随附 skills 安装到 `~/.otto/skills/`。请确认 `~/.local/bin` 已在 `PATH` 中。
Web UI 与 WASM 绑定已迁到 [otto-plus](https://github.com/baiyuqing/otto-plus)，
Otto 构建不再依赖 npm 或前端产物。

如果只想在当前 checkout 中保留本地二进制，可运行 `make build`，然后执行
`./otto --help`。

如果不想每次重敲启动参数，可复制启动器模板，改用脚本启动：

```bash
cp otto.env.example otto.env   # provider、model、工作区、沙箱
make run                       # 等价于 scripts/otto.sh
```

`otto.env` 不会进入 Git，按 shell profile 的方式加载；详见
[启动脚本](docs/user-manual.md#launcher-script)。

## 在 Mac 上使用 Docker

启动 Docker Desktop 后，在仓库目录运行：

```bash
docker compose build
docker compose run --rm otto setup
docker compose up -d
docker compose logs otto
```

在 Mac 浏览器中打开日志中的 `http://127.0.0.1:8787/?token=...` 地址。
镜像会按本机架构构建，支持 M 系列和 Intel Mac，包含完整 Web UI。
配置和会话保存在命名卷中；可通过 `OTTO_WORKSPACE` 指定挂载的工作目录。
使用 API key 时，在 setup 中将环境变量名选为 `OTTO_API_KEY`，并在启动
Compose 前导出该变量。CLI 用法、指定架构、停止和升级步骤见
[Docker 使用说明](docs/user-manual.md#docker-on-mac)。容器内运行 Linux，
没有 macOS Seatbelt；执行 Bash 需要显式指定 `--sandbox off`。

## 快速开始

安装后运行 `otto setup` 创建默认 profile。它不会保存 API key，Shell 命令默认仍在沙箱中运行。模型 ID 必须由你从自己的账号或服务商中选择，Otto 不会预设模型。

```bash
./otto setup
```

使用 ChatGPT 时，接着登录并启动 Otto：

```bash
./otto login
./otto
```

使用 OpenAI-compatible 服务商时，导出 setup 选择的环境变量后启动：

```bash
export OPENAI_API_KEY=your-key
./otto
```

也可以不创建 profile，继续使用一次性的 provider 参数。

### ChatGPT 登录

登录后，将 `YOUR_MODEL_ID` 替换为你的账号可用的模型名称：

```bash
./otto login
./otto --provider chatgpt --model YOUR_MODEL_ID
```

### OpenAI-compatible API

在 Shell 中将 API key 导出为 `OTTO_API_KEY` 环境变量。
将示例地址和模型名称替换为服务提供方的实际配置；接口必须支持流式 Chat Completions。

```bash
./otto --provider openai-compatible --base-url https://example.invalid/v1 --model YOUR_MODEL_ID
```

API key 从配置中的 `api_key_env` 指定变量读取，回退变量为 `OTTO_API_KEY`。
没有 `--api-key` 参数，也不要把密钥写入 TOML。长期使用可设置
[默认 profile](docs/user-manual.md#configuration)。

## 试一次任务

选择 Otto 要使用的工作区：

```bash
./otto --provider chatgpt --model YOUR_MODEL_ID --cwd /path/to/workspace
```

进入交互界面后，可以依次输入这些任务示例：

```text
帮我安排今天的计划，并写一份简短的邀请。
阅读这个工作区，说明项目的组织方式。
五分钟后提醒我跟进。
记住我喜欢简洁的进度更新。
```

配置默认 profile 后，也可以运行一次任务并退出，或继续最近的会话：

```bash
./otto --prompt "总结这个工作区是做什么的"
./otto --continue
```

输入 `/help` 查看交互命令。

TUI 中可用 `/image <path>` 将一张 PNG、JPEG 或 WebP 图片附加到下一条提示词；
Web UI 可点击 **Image** 或粘贴截图。所选模型和 OpenAI-compatible 端点必须支持图片输入。

## 更多用法

- [会话与归档](docs/user-manual.md#sessions)
- [本地记忆与反思](docs/user-manual.md#memory)：反思会生成供你审核的记忆候选，也可按证据和安全检查写入可撤销的 Skills。
- [Skills](docs/user-manual.md#skills)：反思可将成功执行过的流程沉淀成 `SKILL.md` 和 Python 脚本，经语法检查与审批后发布；`/skill revert` 可一起回滚说明和代码。
- [子代理](README.md#delegate-work-to-sub-agents)
- [本地服务：otto serve](docs/user-manual.md#agent-server)
- [ACP agent：otto acp](docs/user-manual.md#acp-agent-server)
- [聊天连接器 otto-connect](docs/user-manual.md#chat-connector)：`chat-connector/` 中独立的 Go 程序，运行 `otto acp` 并接入 Telegram 和飞书；可连接 `otto serve`，与 Web UI 和附加终端共享会话。也可用 `scripts/otto-tcp.sh` 直接走 ACP TCP：由 `otto serve --acp-listen` 充当 agent，连接器不再自行启动进程，脚本负责启动、重启、查看状态与日志。
- [MCP 服务](docs/user-manual.md#mcp-servers)：连接 stdio 或 HTTP 的 MCP 服务，在同一轮循环中调用它们的工具
- [定时提醒](docs/user-manual.md#remind) · [持久化 workflow](docs/user-manual.md#durable-workflows)：TOML 描述的 DAG，支持顺序/并发的 agent 与 handoff 步骤、持久化审批、重启恢复、CLI 控制和 Web UI 查看
- [用量历史](docs/user-manual.md#observability)：Web UI 分析页展示本地 token 趋势和缓存命中率，不保存提示词或工具内容
- [命令参考](docs/user-manual.md#command-line-reference)与[问题排查](docs/user-manual.md#troubleshooting)

## 安全与限制

provider 仅支持 `openai-compatible` 和 `chatgpt`。
**macOS** 是受支持的平台；在 **Linux** 上 Otto 能编译能运行，但没有沙箱：
`auto` 和 `seatbelt` 会失败关闭，`bash` 工具根本不会注册，只有显式传
`--sandbox off` 才能执行命令，且完全不受约束。其余功能（文件工具、会话、
记忆、Skills、子代理、MCP、workflow、`otto serve` 和 Web UI、TUI）两个平台一致。
文件工具限定在工作区内；`--sandbox off` 会显式关闭 Shell 沙箱。
Seatbelt 不是虚拟机，也不能阻止对可写工作区内文件的破坏。
会话文件可能包含工作区文件、提示词、图片和工具结果，应按敏感数据处理。

会话默认使用 **AUTO** 智能审批：工作区内的受限操作直接执行，额外权限请求依据原始用户任务和具体操作接受独立审核。明确的人工检查点、问题和安全权限修改仍需你决定；审核期间不弹审批卡片或窗口。`/auto off` 切换人工审批，`/auto on` 恢复 AUTO；`/permissions policy never` 无提示拒绝额外权限，且不关闭沙箱。设置在恢复会话后保留。升级后需重启 Otto、`otto serve` 和连接器；审批数据库升级到版本 4，降级二进制前须恢复升级备份。详见[统一审批](docs/user-manual.md#unified-approvals)。

反思生成的 Skills 会经过证据与安全检查后写入，并可用 `/skill revert` 撤销。当前不支持嵌套子代理。
其他限制见[英文 README](README.md#safety-and-limitations)，完整访问规则见
[工具与安全](docs/user-manual.md#tools-and-safety)。

## 参与贡献

代码是一个包含两个 crate 的 Cargo workspace：

- `crates/otto-core`：provider 契约、wire 编解码、会话编解码、agent 循环和配置，可编译到 `wasm32-unknown-unknown`。
- `crates/otto`：原生二进制，包含 CLI、REPL、TUI、工具、沙箱、记忆、Skills、子代理、`otto serve` 和 `otto acp`。
- [otto-plus](https://github.com/baiyuqing/otto-plus) 独立维护浏览器前端和 WASM 绑定，通过固定 revision 复用共享 core。

开发约定和检查命令见 [AGENTS.md](AGENTS.md)，包契约见[开发指南（英文）](docs/development.md)。
`make check` 需要 `wasm-pack` 0.15 和 Node 24+。Go 实现的替换原因见
[Rust 重写计划](docs/specs/2026-09-13-rust-rewrite-plan.md)，最后一个 Go 版本的 tag 为 `go-final`。

## 许可证

[MIT](LICENSE)。
