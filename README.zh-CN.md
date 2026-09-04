<p align="center">
  <img src="docs/logo.png" alt="Otto 标志" width="320">
</p>

# Otto — macOS 终端 AI 编程助手

[English](README.md) · [用户手册（英文）](docs/user-manual.md)

**Otto 是一个使用 Go 编写、优先使用本地存储的 AI coding agent。**
在终端中阅读代码、修改文件、运行测试，并在后续会话中继续工作。
支持通过 API key 连接 OpenAI-compatible 接口，或通过 `otto login`
登录使用 ChatGPT provider。模型请求会发送给所选服务；运行时、会话历史和记忆存储位于本地。

- **在终端中完成开发任务：** 内联 TUI、面向管道的 REPL，以及用于脚本的非交互模式。
- **限定访问范围：** 文件工具限制在工作区内，Shell 命令默认通过 macOS Seatbelt 沙箱执行。
- **延续工作上下文：** 持久化会话、上下文压缩、本地记忆、可复用 Skills 和有明确边界的子代理任务。

## 从源码安装

需要 **macOS、Go 1.26 或更新版本**，以及受支持服务的访问权限。

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
go build -trimpath -o ./otto ./cmd/otto
./otto --help
```

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

## 尝试一个开发任务

选择要处理的项目目录：

```bash
./otto --provider chatgpt --model YOUR_MODEL_ID --cwd /path/to/project
```

进入交互界面后，可以依次输入这些任务示例：

```text
解释这个仓库的入口，以及如何运行测试。
为刚才发现的问题添加一个失败测试，再做最小修复。
运行相关测试，并总结 diff。
```

配置默认 profile 后，也可以运行一次任务并退出，或继续最近的会话：

```bash
./otto --approve "总结这个仓库中的 TODO"
./otto --continue
```

输入 `/help` 查看交互命令。

## 更多用法

- [会话与归档](docs/user-manual.md#sessions)
- [本地记忆](docs/user-manual.md#memory)
- [Skills](docs/user-manual.md#skills)
- [子代理](README.md#delegate-work-to-sub-agents)
- [本地服务：otto serve](docs/user-manual.md#agent-server)
- [命令参考](docs/user-manual.md#command-line-reference)与[问题排查](docs/user-manual.md#troubleshooting)

## 安全与限制

仅支持 macOS，provider 为 `openai-compatible` 和 `chatgpt`。
文件工具限定在工作区内；`--sandbox off` 会显式关闭 Shell 沙箱。
Seatbelt 不是虚拟机，也不能阻止对可写工作区内文件的破坏。
会话文件可能包含源代码、提示词和工具结果，应按项目敏感数据处理。

不支持插件、自动发现项目配置、嵌套子代理，以及自动记忆提取。
其他限制见[英文 README](README.md#safety-and-limitations)，完整访问规则见
[工具与安全](docs/user-manual.md#tools-and-safety)。

## 参与贡献

开发约定和检查命令见 [AGENTS.md](AGENTS.md)。

## 许可证

[MIT](LICENSE)。
