//! Builder connection of MCP servers.

use super::*;

impl Builder {
    pub fn connecting_mcp_servers(&self) -> Arc<mcp::Servers> {
        let servers = Arc::new(mcp::Servers::default());
        if !self.mcp.enabled {
            return servers;
        }
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        for server in self.mcp.servers.iter().chain(plugins.servers.iter()) {
            servers.push(
                mcp::ServerStatus {
                    name: plugins
                        .guards
                        .get(&server.name)
                        .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                    transport: transport_label(&server.transport),
                    era: None,
                    state: if server.enabled {
                        mcp::ServerState::Connecting
                    } else {
                        mcp::ServerState::Disabled
                    },
                },
                None,
            );
        }
        servers
    }

    /// Connects enabled MCP servers concurrently, then processes every outcome
    /// in configuration order so `warnings`, `/mcp` status, and cross-server
    /// tool-name deduplication stay stable and reproducible. Each server's
    /// outcome is pushed to the returned [`mcp::Servers`] handle for
    /// `/mcp`; a disabled or failed server never stops the others, and never
    /// fails the build. Restarting an exited stdio server is out of scope; a
    /// server that later exits stays `Connected` until the process restarts.
    /// A server whose configured secrets already exceed
    /// [`dynamic_redaction_marker`]'s limits is reported `Failed` without
    /// attempting to connect, so an oversized secret never falls back to
    /// blanking every tool result with an empty marker (Finding 5b).
    ///
    /// Returns the parent's tools, the connected servers (for
    /// [`mcp_child_tools`], since [`mcp::client::Client`] tools cannot be
    /// cloned into a second registry), and the status handle.
    pub async fn connect_mcp(
        &self,
        max_output: usize,
        warnings: &mut (dyn Write + Send),
    ) -> (
        Vec<Box<dyn Tool + Send + Sync>>,
        Vec<ConnectedServer>,
        Arc<mcp::Servers>,
    ) {
        self.connect_mcp_with_grace(max_output, mcp::DEFAULT_CANCELLATION_GRACE, warnings)
            .await
    }

    /// Connects MCP servers using the runtime cooperative cancellation grace.
    pub async fn connect_mcp_with_grace(
        &self,
        max_output: usize,
        cancellation_grace: Duration,
        warnings: &mut (dyn Write + Send),
    ) -> (
        Vec<Box<dyn Tool + Send + Sync>>,
        Vec<ConnectedServer>,
        Arc<mcp::Servers>,
    ) {
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        self.connect_mcp_with_plugins(max_output, cancellation_grace, warnings, &plugins)
            .await
    }

    pub(crate) async fn connect_mcp_with_plugins(
        &self,
        max_output: usize,
        cancellation_grace: Duration,
        warnings: &mut (dyn Write + Send),
        plugins: &crate::plugin::Snapshot,
    ) -> (
        Vec<Box<dyn Tool + Send + Sync>>,
        Vec<ConnectedServer>,
        Arc<mcp::Servers>,
    ) {
        let servers = Arc::new(mcp::Servers::default());
        let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
        let mut connected: Vec<ConnectedServer> = Vec::new();
        if !self.mcp.enabled {
            return (tools, connected, servers);
        }

        let cancel = CancellationToken::new();
        let connect_timeout = Duration::from_secs(self.mcp.connect_timeout_secs);
        let call_timeout = Duration::from_secs(self.mcp.call_timeout_secs);
        let outcomes = join_all(
            self.mcp
                .servers
                .iter()
                .chain(plugins.servers.iter())
                .cloned()
                .map(|server| {
                    let cancel = cancel.clone();
                    async move {
                        let transport = transport_label(&server.transport);
                        if !server.enabled {
                            return McpConnectOutcome {
                                server,
                                transport,
                                result: McpConnectResult::Disabled,
                            };
                        }
                        let Some(marker) = dynamic_redaction_marker(&server.secrets) else {
                            return McpConnectOutcome {
                                server,
                                transport,
                                result: McpConnectResult::RedactionLimit,
                            };
                        };
                        let result = if plugins.guards.get(&server.name).is_some_and(|g| !g.valid())
                        {
                            Err(mcp::CallError::Transport(
                                "plugin authorization changed before connection".into(),
                            ))
                        } else {
                            self.connect_one(
                                &server,
                                connect_timeout,
                                call_timeout,
                                cancellation_grace,
                                &cancel,
                            )
                            .await
                        };
                        let result = match result {
                            Ok((client, bearer)) => McpConnectResult::Connected(client, bearer),
                            Err(mcp::CallError::NeedsLogin) => McpConnectResult::NeedsLogin,
                            Err(error) => McpConnectResult::Failed(failure_message(
                                &error,
                                &server.secrets,
                                &marker,
                            )),
                        };
                        McpConnectOutcome {
                            server,
                            transport,
                            result,
                        }
                    }
                }),
        )
        .await;

        let mut registered_names: HashSet<String> = HashSet::new();
        for outcome in outcomes {
            let server = outcome.server;
            let transport_kind = outcome.transport;
            match outcome.result {
                McpConnectResult::Disabled => {
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::Disabled,
                        },
                        None,
                    );
                }
                McpConnectResult::RedactionLimit => {
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::Failed(SECRETS_EXCEED_REDACTION_LIMITS.into()),
                        },
                        None,
                    );
                    self.log_mcp_failed(
                        &server.name,
                        transport_kind,
                        SECRETS_EXCEED_REDACTION_LIMITS,
                    );
                    let _ = writeln!(
                        warnings,
                        "warning: mcp server {:?} failed to connect: {SECRETS_EXCEED_REDACTION_LIMITS}",
                        server.name
                    );
                }
                McpConnectResult::Connected(client, bearer) => {
                    let client = Arc::new(client);
                    let tool_server: Arc<dyn mcp::ToolServer> =
                        if let Some(authorization) = plugins.guards.get(&server.name) {
                            if !authorization.valid() {
                                client.close().await;
                                let _ = writeln!(
                                    warnings,
                                    "warning: plugin {} authorization changed during connection",
                                    authorization.logical_name
                                );
                                servers.push(
                                    mcp::ServerStatus {
                                        name: authorization.logical_name.clone(),
                                        transport: transport_kind,
                                        era: None,
                                        state: mcp::ServerState::Failed(
                                            "plugin authorization changed".into(),
                                        ),
                                    },
                                    None,
                                );
                                continue;
                            }
                            authorization.watch(&client, &servers);
                            Arc::new(crate::plugin::GuardedServer::new(
                                Arc::clone(&client),
                                authorization.clone(),
                            ))
                        } else {
                            Arc::clone(&client) as Arc<dyn mcp::ToolServer>
                        };
                    let mut secrets = server.secrets.clone();
                    if let Some(bearer) = &bearer {
                        for secret in bearer.secrets() {
                            if !secret.is_empty() && !secrets.contains(&secret) {
                                secrets.push(secret);
                            }
                        }
                    }
                    let (server_tools, tool_warnings) = tools_for(
                        Arc::clone(&tool_server),
                        client.tools(),
                        max_output,
                        secrets.clone(),
                        bearer.clone(),
                    );
                    for warning in &tool_warnings {
                        let _ = writeln!(warnings, "warning: {warning}");
                    }
                    let (keep, dedup_warnings) =
                        dedup_cross_server(&server.name, &server_tools, &mut registered_names);
                    for warning in &dedup_warnings {
                        let _ = writeln!(warnings, "warning: {warning}");
                    }
                    let keep: HashSet<usize> = keep.into_iter().collect();
                    let tool_names: Vec<String> = server_tools
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| keep.contains(index))
                        .map(|(_, tool)| tool.definition().name)
                        .collect();
                    self.logger.info(
                        "mcp_server_connected",
                        &[
                            ("server", server.name.clone()),
                            ("transport", transport_kind.to_string()),
                            ("tools", tool_names.len().to_string()),
                        ],
                    );
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: Some(client.era().clone()),
                            state: mcp::ServerState::Connected {
                                tools: tool_names.len(),
                            },
                        },
                        Some(Arc::clone(&client)),
                    );
                    connected.push(ConnectedServer {
                        server: tool_server,
                        tool_infos: client
                            .tools()
                            .iter()
                            .cloned()
                            .map(|mut info| {
                                if let Some(g) = plugins.guards.get(&server.name) {
                                    info.description = Some(format!(
                                        "[{}] {}",
                                        g.logical_name,
                                        info.description.as_deref().unwrap_or(&info.name)
                                    ));
                                }
                                info
                            })
                            .collect(),
                        secrets,
                        bearer,
                        tool_names,
                    });
                }
                McpConnectResult::NeedsLogin => {
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::NeedsLogin,
                        },
                        None,
                    );
                    let _ = writeln!(
                        warnings,
                        "warning: mcp server {:?} needs login: run 'otto mcp login {}'",
                        server.name, server.name
                    );
                }
                McpConnectResult::Failed(message) => {
                    self.log_mcp_failed(&server.name, transport_kind, &message);
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::Failed(message.clone()),
                        },
                        None,
                    );
                    let _ = writeln!(
                        warnings,
                        "warning: mcp server {:?} failed to connect: {message}",
                        server.name
                    );
                }
            }
        }
        tools.extend(mcp_router_tools(&connected, max_output));
        (tools, connected, servers)
    }

    /// `message` is the redacted text `/mcp` shows for the failure.
    fn log_mcp_failed(&self, server: &str, transport: &str, message: &str) {
        self.logger.warn(
            "mcp_server_failed",
            &[
                ("server", server.to_string()),
                ("transport", transport.to_string()),
                ("error", message.to_string()),
            ],
        );
    }

    /// Builds the transport for one configured server and connects it.
    /// Returns the OAuth bearer alongside the client, when the server uses
    /// one, so `connect_mcp` can redact its current tokens from tool results
    /// (Finding 6) without `crate::mcp::oauth` gaining a caller outside tests.
    async fn connect_one(
        &self,
        server: &otto_core::config::McpServerRuntime,
        connect_timeout: Duration,
        call_timeout: Duration,
        cancellation_grace: Duration,
        cancel: &CancellationToken,
    ) -> Result<(mcp::client::Client, Option<Arc<dyn mcp::BearerSource>>), mcp::CallError> {
        if crate::tool::project_guard::Guard::for_config(
            &self.config,
            &self.home,
            &self.workspace_path,
        )
        .is_some_and(|g| !g.valid())
        {
            return Err(mcp::CallError::Transport(
                "project executable trust was revoked before connection".into(),
            ));
        }
        match &server.transport {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                if self
                    .config
                    .policy
                    .as_ref()
                    .and_then(|p| p.allowed_mcp_commands.as_ref())
                    .is_some()
                {
                    // Policies name immutable administrator-controlled binaries, never a
                    // PATH lookup or a workspace-owned file at an allowlisted spelling.
                    let path = Path::new(command);
                    for ancestor in path.ancestors() {
                        use std::os::unix::fs::MetadataExt;
                        let info = std::fs::symlink_metadata(ancestor).map_err(|_| {
                            mcp::CallError::Transport("cannot inspect managed MCP command".into())
                        })?;
                        if info.file_type().is_symlink()
                            || info.uid() != 0
                            || info.mode() & 0o022 != 0
                        {
                            return Err(mcp::CallError::Transport("managed MCP command must be administrator-owned and not user-writable".into()));
                        }
                    }
                }
                let transport = mcp::stdio::StdioTransport::spawn_with_grace(
                    command,
                    args,
                    env,
                    Path::new(cwd),
                    cancellation_grace,
                )
                .await?;
                let client = mcp::client::Client::connect_with_grace(
                    server.name.clone(),
                    Box::new(transport),
                    connect_timeout,
                    call_timeout,
                    cancellation_grace,
                    cancel,
                )
                .await?;
                Ok((client, None))
            }
            McpTransport::Http {
                url, headers, auth, ..
            } => {
                let bearer: Option<Arc<dyn mcp::BearerSource>> = match auth {
                    McpAuth::OAuth => Some(Arc::new(mcp::oauth::TokenStore::new(
                        server.name.clone(),
                        mcp::oauth::token_path(Path::new(&self.home), &server.name),
                    ))),
                    McpAuth::None => None,
                };
                let transport = mcp::http::HttpTransport::new(
                    url.clone(),
                    headers.clone(),
                    bearer.clone(),
                    call_timeout,
                )?;
                let client = mcp::client::Client::connect_with_grace(
                    server.name.clone(),
                    Box::new(transport),
                    connect_timeout,
                    call_timeout,
                    cancellation_grace,
                    cancel,
                )
                .await?;
                Ok((client, bearer))
            }
        }
    }
}
