<p align="center">
  <img src="docs/logo.svg?v=2" alt="Otto 标志" width="320">
</p>

# Otto — 本地优先的通用个人 AI Agent

[English](README.md) · [文档索引（英文）](docs/README.md) · [更新日志（英文）](CHANGELOG.md)

Otto 帮你研究、写作、规划、编程和处理日常任务。可从终端、独立的
[otto-plus](https://github.com/baiyuqing/otto-plus) 浏览器客户端，或通过
`otto-connect` 从 Telegram、飞书使用。连接 `otto serve` 后，各客户端可以共享服务端会话。

- **保持上下文：** 持久化会话、记忆与可复用 skills。
- **委派与自动化：** 子代理、定时任务和可恢复的持久化 workflow。
- **限定访问：** 工作区文件边界、审批与 macOS Shell 沙箱。

支持第一方 **Claude API**、**OpenAI-compatible** 接口和 **ChatGPT** 登录。
模型请求发送给所选服务；“本地优先”指运行时、会话历史和默认记忆存储在本地，
不表示模型在本地运行。

## 从源码安装

通过 rustup 安装 Rust（版本由 [`rust-toolchain.toml`](rust-toolchain.toml) 锁定），
并安装 [`chat-connector/go.mod`](chat-connector/go.mod) 要求的 Go 版本。
macOS 是受支持的平台；Linux 可以编译运行，但没有原生 Shell 沙箱。

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make install
otto --version
otto setup
```

`make install` 构建 Otto 和 `otto-connect`，安装到 `~/.local/bin/`，并将随附
skills 安装到 `~/.otto/skills/`。请将 `~/.local/bin` 加入 `PATH`。
只在 checkout 内构建可用 `make build`，然后运行 `./otto`。

## 首次使用

`otto setup` 创建配置，不保存 API key。请选择你的账号或服务实际可用的模型。

使用 ChatGPT：

```bash
otto login
otto
```

使用 Claude 或 OpenAI-compatible 服务：在 setup 中选择模型和 API-key 环境变量名；
OpenAI-compatible profile 还需选择接口地址。启动前在进程环境中私下设置该变量。
不要将密钥写进配置文件或提示词。

可以先让 Otto 解释一个仓库、查证一个问题，或完成一个小改动。
`/help` 查看命令，`/new` 开始新会话。

源码安装的升级方式：更新 checkout，停止所有运行中的 Otto 进程（包括 `otto serve`
和 connector），重新执行 `make install`，再启动它们；新旧二进制不能同时使用
`~/.otto/`。升级前先查看[更新日志](CHANGELOG.md)中的不兼容变更。配置、单次 CLI 参数、启动脚本与 Docker 用法见
[入门指南](docs/manual/getting-started.md)。

## 按任务查文档

以下详细指南为英文，当前行为只在对应主题维护。

| 目标 | 指南 |
| --- | --- |
| 终端交互、会话和脚本模式 | [交互使用](docs/manual/interactive-use.md) |
| 审批、沙箱、工具和子代理 | [安全与工具](docs/manual/safety-and-tools.md) |
| 浏览器/API、ACP、Telegram 或飞书 | [服务端与连接器](docs/manual/servers-and-connectors.md) |
| 持久化 workflow 与工作区身份 | [自动化](docs/manual/automation.md) |
| 记忆、skills、MCP 和扩展 | [记忆与扩展](docs/manual/memory-and-extensions.md) |
| 在 Rust 应用中嵌入 Otto | [嵌入 API](docs/development/architecture.md#embedding-api) |
| 排查故障 | [故障排查](docs/manual/troubleshooting.md) |

## 安全与限制

- 文件工具限定在选定工作区内。macOS 的 Bash 默认使用 Seatbelt 沙箱；Linux
  和容器没有这个边界，执行 Shell 需要显式关闭沙箱。
- 审批不等于沙箱授权。审批内容可能显示并持久化，不要放入秘密。API key 和聊天
  密钥仅放在环境变量中；ChatGPT 凭据通过 `otto login` 获取。
- 模型服务、记忆插件和配置的集成可能接收任务内容，处理敏感数据前请检查配置。
  每次请求模型服务前，Otto 在本地替换已知凭据、高置信度的密钥格式、邮箱地址、
  以 `+` 开头的电话号码、银行卡号和中国居民身份证号；其他个人信息按原样发送。见
  [出站密钥与 PII 检查](docs/manual/safety-and-tools.md#provider-egress-secret-and-pii-guard)。
- `otto serve` 默认监听 Unix socket。非 loopback 的 TCP 监听需要 TLS
  （`[server].tls_cert`、`[server].tls_key`）和 `OTTO_SERVER_TOKEN`，见
  [Listener](docs/manual/servers-and-connectors.md#listener)。Release 构建在 panic
  时终止整个进程，长期运行的服务端应由会自动重启的 supervisor 托管。
- 已获权限内的命令仍会产生真实影响；重要工作请保留备份并审阅修改。

权限模式与恢复行为见[安全与工具](docs/manual/safety-and-tools.md)。Seatbelt
完整验证需要 macOS 和独立 Command Line Tools，Linux 检查不能证明沙箱行为。

## 参与贡献

先读 [`AGENTS.md`](AGENTS.md)、[代码组织](docs/code-organization.md)和
[开发指南](docs/development.md)。使用独立分支与 worktree，安装 Gitleaks 和 Node 后运行
`make setup-git-hooks`，并执行覆盖改动的检查。仓库没有 CI，PR 必须列出本地验证结果。

[架构图](docs/architecture/otto-architecture.svg) · [许可证](LICENSE)
