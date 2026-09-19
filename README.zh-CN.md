<p align="center">
  <img src="docs/logo.png" alt="Otto 标志" width="320">
</p>

# Otto — macOS 上的本地优先 Agent

[English](README.md) · [用户手册（英文）](docs/user-manual.md)

**Otto 是一个使用 Rust 编写、优先使用本地存储的 Agent。**
给它一项任务，它会循环调用模型、按需使用工具，并在需要时压缩上下文。
支持通过 API key 连接 OpenAI-compatible 接口，或通过 `otto login`
登录使用 ChatGPT provider。模型请求会发送给所选服务；运行时、会话历史和记忆存储位于本地。

- **在终端里工作：** 全屏 TUI、面向管道的 REPL，以及用于脚本的非交互模式。
- **限定访问范围：** 文件工具限制在工作区内，Shell 命令默认通过 macOS Seatbelt 沙箱执行。
- **不必一直盯着：** 持久化会话、本地记忆、可复用 Skills、有边界的子代理、定时器；`otto serve` 还可选把飞书消息送进会话 inbox。

## 从源码安装

需要 **macOS**、由 `rust-toolchain.toml` 锁定的 **Rust 1.98** 工具链
（`rustup toolchain install` 会自动安装）、**Node 24+**、`wasm-pack` 0.15，
以及受支持服务的访问权限。

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make build
./otto --help
```

`make build` 会先刷新并嵌入 Web UI，再编译 Otto。若直接运行 `cargo build` 且
此前未执行 `make ui`，二进制中只会嵌入一行占位文本。

以下命令在该目录中运行。将二进制文件放入 `PATH` 后，可以在其他目录直接使用 `otto`。

## 快速开始

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
这个工作区里有什么，我接下来该做什么？
五分钟后提醒我跟进。
记住我们决定先做 inbox 这条路径。
```

配置默认 profile 后，也可以运行一次任务并退出，或继续最近的会话：

```bash
./otto --approve "总结这个工作区是做什么的"
./otto --continue
```

输入 `/help` 查看交互命令。

TUI 中可用 `/image <path>` 将一张 PNG、JPEG 或 WebP 图片附加到下一条提示词；
Web UI 可点击 **Image** 或粘贴截图。所选模型和 OpenAI-compatible 端点必须支持图片输入。

## 更多用法

- [会话与归档](docs/user-manual.md#sessions)
- [本地记忆](docs/user-manual.md#memory)
- [Skills](docs/user-manual.md#skills)
- [子代理](README.md#delegate-work-to-sub-agents)
- [本地服务：otto serve](docs/user-manual.md#agent-server)，可选[飞书 inbound](docs/user-manual.md#feishu-inbound)
- [用量历史](docs/user-manual.md#observability)：Web UI 分析页展示本地 token 趋势和缓存命中率，不保存提示词或工具内容
- [命令参考](docs/user-manual.md#command-line-reference)与[问题排查](docs/user-manual.md#troubleshooting)

## 安全与限制

仅支持 macOS，provider 为 `openai-compatible` 和 `chatgpt`。
文件工具限定在工作区内；`--sandbox off` 会显式关闭 Shell 沙箱。
Seatbelt 不是虚拟机，也不能阻止对可写工作区内文件的破坏。
会话文件可能包含工作区文件、提示词、图片和工具结果，应按敏感数据处理。

不支持插件、自动发现项目配置、嵌套子代理，以及自动记忆提取。
其他限制见[英文 README](README.md#safety-and-limitations)，完整访问规则见
[工具与安全](docs/user-manual.md#tools-and-safety)。

## 参与贡献

代码是一个包含三个 crate 的 Cargo workspace：

- `crates/otto-core`：provider 契约、wire 编解码、会话编解码、agent 循环和配置，可编译到 `wasm32-unknown-unknown`。
- `crates/otto`：macOS 二进制，包含 CLI、REPL、TUI、工具、沙箱、记忆、Skills、子代理、inbound 适配器和 `otto serve`。
- `crates/otto-web`：把 `otto-core` 编译为 WebAssembly 供 `ui/` 中的浏览器前端使用，前端与二进制共用同一份实现。

开发约定和检查命令见 [AGENTS.md](AGENTS.md)，包契约见[开发指南（英文）](docs/development.md)。
`make check` 需要 `wasm-pack` 0.15 和 Node 24+。Go 实现的替换原因见
[Rust 重写计划](docs/specs/2026-09-13-rust-rewrite-plan.md)，最后一个 Go 版本的 tag 为 `go-final`。

## 许可证

[MIT](LICENSE)。
