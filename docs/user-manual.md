# Otto User Manual

This stable index links to the current user documentation. The manual is split into focused pages so that each topic has one canonical location.

## Guides

- [Getting started](manual/getting-started.md) — Prerequisites, Quick start, ChatGPT subscription, Command-line reference, Environment variables, Configuration.
- [Interactive use](manual/interactive-use.md) — Frontends, Slash commands, Sessions, Context compaction, Headless mode.
- [Safety and tools](manual/safety-and-tools.md) — Unified approvals, Tools and safety.
- [Servers and connectors](manual/servers-and-connectors.md) — Agent server, ACP agent server, Chat connector, Logs, metrics, and request correlation.
- [Automation](manual/automation.md) — Durable workflows, Workspace identity.
- [Memory and extensions](manual/memory-and-extensions.md) — Memory, Skills, MCP servers, Plugin packages, Hooks and custom tools.
- [Remote memory protocol](manual/remote-memory-protocol.md) — Wire format of the remote memory service.
- [Troubleshooting](manual/troubleshooting.md) — Troubleshooting.

## Legacy deep-link map

Older external links to `user-manual.md#…` no longer land directly on the moved section. Use this map to navigate to the new canonical page and anchor.

| Previous anchor | New location |
| --- | --- |
| `#prerequisites` | [Prerequisites](manual/getting-started.md#prerequisites) |
| `#platform-support` | [Platform support](manual/getting-started.md#platform-support) |
| `#docker-on-mac` | [Docker on Mac](manual/getting-started.md#docker-on-mac) |
| `#quick-start` | [Quick start](manual/getting-started.md#quick-start) |
| `#launcher-script` | [Launcher script](manual/getting-started.md#launcher-script) |
| `#chatgpt-subscription` | [ChatGPT subscription](manual/getting-started.md#chatgpt-subscription) |
| `#signing-in` | [Signing in](manual/getting-started.md#signing-in) |
| `#using-the-subscription` | [Using the subscription](manual/getting-started.md#using-the-subscription) |
| `#how-it-works` | [How it works](manual/getting-started.md#how-it-works) |
| `#command-line-reference` | [Command-line reference](manual/getting-started.md#command-line-reference) |
| `#environment-variables` | [Environment variables](manual/getting-started.md#environment-variables) |
| `#configuration` | [Configuration](manual/getting-started.md#configuration) |
| `#inspecting-and-editing-scopes` | [Inspecting and editing scopes](manual/getting-started.md#inspecting-and-editing-scopes) |
| `#project-executable-trust` | [Project executable trust](manual/getting-started.md#project-executable-trust) |
| `#managed-policy` | [Managed policy](manual/getting-started.md#managed-policy) |
| `#precedence` | [Precedence](manual/getting-started.md#precedence) |
| `#backups-of-the-config-file` | [Backups of the config file](manual/getting-started.md#backups-of-the-config-file) |
| `#frontends` | [Frontends](manual/interactive-use.md#frontends) |
| `#tui-behavior` | [TUI behavior](manual/interactive-use.md#tui-behavior) |
| `#tui-keys` | [TUI keys](manual/interactive-use.md#tui-keys) |
| `#the-tui-attached-to-otto-serve` | [The TUI attached to `otto serve`](manual/interactive-use.md#the-tui-attached-to-otto-serve) |
| `#repl-behavior` | [REPL behavior](manual/interactive-use.md#repl-behavior) |
| `#herdr` | [Herdr](manual/interactive-use.md#herdr) |
| `#slash-commands` | [Slash commands](manual/interactive-use.md#slash-commands) |
| `#sessions` | [Sessions](manual/interactive-use.md#sessions) |
| `#continuing-a-session-on-another-host` | [Continuing a session on another host](manual/interactive-use.md#continuing-a-session-on-another-host) |
| `#optional-pi-interoperability-probe` | [Optional Pi interoperability probe](manual/interactive-use.md#optional-pi-interoperability-probe) |
| `#context-compaction` | [Context compaction](manual/interactive-use.md#context-compaction) |
| `#model-window-metadata` | [Model window metadata](manual/interactive-use.md#model-window-metadata) |
| `#headless-mode` | [Headless mode](manual/interactive-use.md#headless-mode) |
| `#unified-approvals` | [Unified approvals](manual/safety-and-tools.md#unified-approvals) |
| `#tools-and-safety` | [Tools and safety](manual/safety-and-tools.md#tools-and-safety) |
| `#file-tools` | [File tools](manual/safety-and-tools.md#file-tools) |
| `#remind` | [`remind`](manual/safety-and-tools.md#remind) |
| `#schedule` | [`schedule`](manual/safety-and-tools.md#schedule) |
| `#web-tools` | [Web tools](manual/safety-and-tools.md#web-tools) |
| `#verification-verdicts` | [Verification verdicts](manual/safety-and-tools.md#verification-verdicts) |
| `#reflect` | [`reflect`](manual/safety-and-tools.md#reflect) |
| `#listmodels` | [`list_models`](manual/safety-and-tools.md#listmodels) |
| `#sub-agents` | [Sub-agents](manual/safety-and-tools.md#sub-agents) |
| `#stopping-a-turn-that-makes-no-progress` | [Stopping a turn that makes no progress](manual/safety-and-tools.md#stopping-a-turn-that-makes-no-progress) |
| `#interactive-sandbox-setup` | [Interactive sandbox setup](manual/safety-and-tools.md#interactive-sandbox-setup) |
| `#bash-sandbox-policy` | [`bash` sandbox policy](manual/safety-and-tools.md#bash-sandbox-policy) |
| `#git-metadata-is-read-only-in-the-seatbelt-sandbox` | [Git metadata is read-only in the Seatbelt sandbox](manual/safety-and-tools.md#git-metadata-is-read-only-in-the-seatbelt-sandbox) |
| `#a-failed-command-lists-what-the-sandbox-denied` | [A failed command lists what the sandbox denied](manual/safety-and-tools.md#a-failed-command-lists-what-the-sandbox-denied) |
| `#seatbelt-limitations` | [Seatbelt limitations](manual/safety-and-tools.md#seatbelt-limitations) |
| `#agent-server` | [Agent server](manual/servers-and-connectors.md#agent-server) |
| `#listener` | [Listener](manual/servers-and-connectors.md#listener) |
| `#workspaces` | [Workspaces](manual/servers-and-connectors.md#workspaces) |
| `#cross-workspace-deliveries` | [Cross-workspace deliveries](manual/servers-and-connectors.md#cross-workspace-deliveries) |
| `#turn-queue` | [Turn queue](manual/servers-and-connectors.md#turn-queue) |
| `#ottos-own-help-and-approval-controls` | [Otto's own help and approval controls](manual/servers-and-connectors.md#ottos-own-help-and-approval-controls) |
| `#session-plan` | [Session plan](manual/servers-and-connectors.md#session-plan) |
| `#active-task-guidance-and-model-follow-ups` | [Active-task guidance and model follow-ups](manual/servers-and-connectors.md#active-task-guidance-and-model-follow-ups) |
| `#approvals-inside-a-turn` | [Approvals inside a turn](manual/servers-and-connectors.md#approvals-inside-a-turn) |
| `#web-ui` | [Web UI](manual/servers-and-connectors.md#web-ui) |
| `#http-api` | [HTTP API](manual/servers-and-connectors.md#http-api) |
| `#session-lifetime` | [Session lifetime](manual/servers-and-connectors.md#session-lifetime) |
| `#session-object` | [Session object](manual/servers-and-connectors.md#session-object) |
| `#turn-summary` | [Turn summary](manual/servers-and-connectors.md#turn-summary) |
| `#events` | [Events](manual/servers-and-connectors.md#events) |
| `#errors` | [Errors](manual/servers-and-connectors.md#errors) |
| `#observability` | [Observability](manual/servers-and-connectors.md#observability) |
| `#sub-agent-task-records` | [Sub-agent task records](manual/servers-and-connectors.md#sub-agent-task-records) |
| `#agent-server-shutdown` | [Agent server shutdown](manual/servers-and-connectors.md#agent-server-shutdown) |
| `#examples` | [Examples](manual/servers-and-connectors.md#examples) |
| `#not-supported` | [Not supported](manual/servers-and-connectors.md#not-supported) |
| `#acp-agent-server` | [ACP agent server](manual/servers-and-connectors.md#acp-agent-server) |
| `#methods` | [Methods](manual/servers-and-connectors.md#methods) |
| `#scheduled-items-are-sent-with-ottoscheduledeliver` | [Scheduled items are sent with `_otto/schedule/deliver`](manual/servers-and-connectors.md#scheduled-items-are-sent-with-ottoscheduledeliver) |
| `#compact-and-context-are-slash-commands` | [`/compact` and `/context` are slash commands](manual/servers-and-connectors.md#compact-and-context-are-slash-commands) |
| `#elevated-bash-uses-sessionrequestpermission` | [Elevated `bash` uses `session/request_permission`](manual/servers-and-connectors.md#elevated-bash-uses-sessionrequestpermission) |
| `#background-results-start-a-wake-turn` | [Background results start a wake turn](manual/servers-and-connectors.md#background-results-start-a-wake-turn) |
| `#acp-agent-server-shutdown` | [ACP agent server shutdown](manual/servers-and-connectors.md#acp-agent-server-shutdown) |
| `#--attach-forwards-requests-to-otto-serve` | [`--attach` forwards requests to `otto serve`](manual/servers-and-connectors.md#--attach-forwards-requests-to-otto-serve) |
| `#chat-access-goes-through-otto-connect` | [Chat access goes through otto-connect](manual/servers-and-connectors.md#chat-access-goes-through-otto-connect) |
| `#not-supported-over-acp` | [Not supported over ACP](manual/servers-and-connectors.md#not-supported-over-acp) |
| `#chat-connector` | [Chat connector](manual/servers-and-connectors.md#chat-connector) |
| `#building-and-running` | [Building and running](manual/servers-and-connectors.md#building-and-running) |
| `#chat-connector-configuration` | [Chat connector configuration](manual/servers-and-connectors.md#chat-connector-configuration) |
| `#feishu-app` | [Feishu app](manual/servers-and-connectors.md#feishu-app) |
| `#sharing-sessions-with-otto-serve` | [Sharing sessions with `otto serve`](manual/servers-and-connectors.md#sharing-sessions-with-otto-serve) |
| `#direct-acp-tcp-connection` | [Direct ACP TCP connection](manual/servers-and-connectors.md#direct-acp-tcp-connection) |
| `#the-acp-tcp-stack-script` | [The ACP TCP stack script](manual/servers-and-connectors.md#the-acp-tcp-stack-script) |
| `#admission` | [Admission](manual/servers-and-connectors.md#admission) |
| `#messages-and-replies` | [Messages and replies](manual/servers-and-connectors.md#messages-and-replies) |
| `#commands` | [Commands](manual/servers-and-connectors.md#commands) |
| `#provider-profile-management` | [Provider-profile management](manual/servers-and-connectors.md#provider-profile-management) |
| `#memory-review` | [Memory review](manual/servers-and-connectors.md#memory-review) |
| `#permission-requests` | [Permission requests](manual/servers-and-connectors.md#permission-requests) |
| `#agent-process-exits` | [Agent process exits](manual/servers-and-connectors.md#agent-process-exits) |
| `#not-supported-by-otto-connect` | [Not supported by otto-connect](manual/servers-and-connectors.md#not-supported-by-otto-connect) |
| `#logs-metrics-and-request-correlation` | [Logs, metrics, and request correlation](manual/servers-and-connectors.md#logs-metrics-and-request-correlation) |
| `#log-format` | [Log format](manual/servers-and-connectors.md#log-format) |
| `#following-one-chat-message` | [Following one chat message](manual/servers-and-connectors.md#following-one-chat-message) |
| `#prometheus` | [Prometheus](manual/servers-and-connectors.md#prometheus) |
| `#durable-workflows` | [Durable workflows](manual/automation.md#durable-workflows) |
| `#workspace-identity` | [Workspace identity](manual/automation.md#workspace-identity) |
| `#memory` | [Memory](manual/memory-and-extensions.md#memory) |
| `#rust-memory-service-extensions` | [Rust memory service extensions](manual/memory-and-extensions.md#rust-memory-service-extensions) |
| `#migrating-an-existing-database` | [Migrating an existing database](manual/memory-and-extensions.md#migrating-an-existing-database) |
| `#advice-on-candidates` | [Advice on candidates](manual/memory-and-extensions.md#advice-on-candidates) |
| `#standing-preferences` | [Standing preferences](manual/memory-and-extensions.md#standing-preferences) |
| `#reflection` | [Reflection](manual/memory-and-extensions.md#reflection) |
| `#asking-the-agent-for-a-retrospective` | [Asking the agent for a retrospective](manual/memory-and-extensions.md#asking-the-agent-for-a-retrospective) |
| `#past-runs` | [Past runs](manual/memory-and-extensions.md#past-runs) |
| `#generated-skills` | [Generated skills](manual/memory-and-extensions.md#generated-skills) |
| `#automatic-reflection` | [Automatic reflection](manual/memory-and-extensions.md#automatic-reflection) |
| `#skills` | [Skills](manual/memory-and-extensions.md#skills) |
| `#automatic-contract-check` | [Automatic contract check](manual/memory-and-extensions.md#automatic-contract-check) |
| `#mcp-servers` | [MCP servers](manual/memory-and-extensions.md#mcp-servers) |
| `#plugin-packages` | [Plugin packages](manual/memory-and-extensions.md#plugin-packages) |
| `#hooks-and-custom-tools` | [Hooks and custom tools](manual/memory-and-extensions.md#hooks-and-custom-tools) |
| `#hook-protocol-v1` | [Hook protocol v1](manual/memory-and-extensions.md#hook-protocol-v1) |
| `#custom-tools` | [Custom tools](manual/memory-and-extensions.md#custom-tools) |
| `#troubleshooting` | [Troubleshooting](manual/troubleshooting.md#troubleshooting) |
| `#otto-missing-api-key` | [`otto: missing api key`](manual/troubleshooting.md#otto-missing-api-key) |
| `#otto-missing-baseurl-otto-invalid-baseurl-or-request-failures` | [`otto: missing base_url`, `otto: invalid base_url`, or request failures](manual/troubleshooting.md#otto-missing-baseurl-otto-invalid-baseurl-or-request-failures) |
| `#read-chat-completion-stream-or-stream-ended-without-done` | [`read chat completion stream: ...` or stream ended without `[DONE]`](manual/troubleshooting.md#read-chat-completion-stream-or-stream-ended-without-done) |
| `#warning-bash-is-unavailable-because-the-configured-sandbox-could-not-be-established-` | [`warning: bash is unavailable because the configured sandbox could not be established ...`](manual/troubleshooting.md#warning-bash-is-unavailable-because-the-configured-sandbox-could-not-be-established-) |
| `#no-chatgpt-credentials-run-otto-login` | [`no chatgpt credentials; run 'otto login'`](manual/troubleshooting.md#no-chatgpt-credentials-run-otto-login) |
| `#mcp-reports-failed-or-needs-login` | [`/mcp` reports `failed: ...` or `needs login`](manual/troubleshooting.md#mcp-reports-failed-or-needs-login) |
| `#context-length-or-prompt-size-failures` | [Context-length or prompt-size failures](manual/troubleshooting.md#context-length-or-prompt-size-failures) |
| `#a-session-disappeared-from-resume-or---continue` | [A session disappeared from `/resume` or `--continue`](manual/troubleshooting.md#a-session-disappeared-from-resume-or---continue) |
| `#a-session-takes-too-long-to-start` | [A session takes too long to start](manual/troubleshooting.md#a-session-takes-too-long-to-start) |
