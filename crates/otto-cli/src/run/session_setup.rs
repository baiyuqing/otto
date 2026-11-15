//! Initial session, MCP runner swap and frontend selection helpers for `run_with_extensions`.

use super::*;

pub(super) fn spawn_mcp_runner_swap(
    controller: Arc<Controller>,
    runtime: Runtime,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(session) = controller.current_session_opt() else {
            return;
        };
        let session_id = session.header().id;
        let mut runner = match controller
            .builder()
            .build_runner_quiet(&session, &runtime)
            .await
        {
            Ok(runner) => runner,
            Err(_) => return,
        };
        let info = controller.builder().runtime_info(&runtime);
        loop {
            if cancel.is_cancelled() {
                // The session's resources stay: the active runner uses them.
                runner.close();
                return;
            }
            match controller.replace_runner_if_current(&session_id, runner, info.clone()) {
                Ok(true) | Ok(false) => return,
                Err(message) if message == PROMPT_ACTIVE => {
                    // Rebuild instead of holding a runner that may have stale
                    // context after a long turn. This keeps the swap simple
                    // and only installs a runner built for the post-turn state.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let Some(session) = controller.current_session_opt() else {
                        return;
                    };
                    if session.header().id != session_id {
                        return;
                    }
                    runner = match controller
                        .builder()
                        .build_runner_quiet(&session, &runtime)
                        .await
                    {
                        Ok(next) => next,
                        Err(_) => return,
                    };
                }
                Err(_) => return,
            }
        }
    })
}

pub(super) fn prepare_session_backend(
    repository: &dyn session::SessionRepository,
    root: &Path,
    workspace: &str,
    reference: &str,
    create_lease: Option<u64>,
    listed: bool,
) -> Result<Box<dyn session::PreparedSession>, String> {
    let prepared = if listed {
        repository.prepare_listed(root, workspace, reference, create_lease)
    } else {
        repository.prepare(reference, create_lease)
    }?;
    if let Err(error) =
        otto::runtime::builder::validate_session_workspace(&prepared.info().cwd, workspace)
    {
        let _ = prepared.close();
        return Err(error);
    }
    Ok(prepared)
}

pub(super) async fn activate_initial_session(
    builder: &Builder,
    prepared: Option<Box<dyn otto::session::PreparedSession>>,
    dynamic_content: bool,
    runtime: &Runtime,
) -> Result<(SharedSession, Vec<String>), String> {
    if let Some(prepared) = prepared {
        let prepared = prepared.with_replayable(otto::runtime::retry_safety::is_replayable);
        let (session, warnings) = tokio::task::spawn_blocking(move || prepared.activate())
            .await
            .map_err(|error| format!("session activation task panicked: {error}"))?
            .map_err(|error| builder.redact_error(&error, Some(runtime)))?;
        let messages = warnings
            .into_iter()
            .map(|warning| builder.redact_error(&warning.message, Some(runtime)))
            .collect();
        return Ok((SharedSession::new(session), messages));
    }
    if !dynamic_content {
        let header = Header {
            version: CURRENT_VERSION,
            ..Header::default()
        };
        return Ok((SharedSession::memory(header), Vec::new()));
    }
    builder
        .create_session(runtime)
        .map(|session| (session, Vec::new()))
}

pub(super) fn overrides_from(options: &CliOptions) -> Overrides {
    Overrides {
        legacy_profile: options.profile.clone(),
        provider: options.provider.clone(),
        base_url: options.base_url.clone(),
        model: options.model.clone(),
        thinking: options.thinking.clone(),
        shell_timeout: options.shell_timeout,
        max_output_bytes: options.max_output_bytes,
    }
}

pub(super) fn select_frontend(mode: UiMode, terminal: bool) -> Result<Frontend, String> {
    match mode {
        UiMode::Auto if terminal => Ok(Frontend::Tui),
        UiMode::Auto => Ok(Frontend::Repl),
        UiMode::Repl => Ok(Frontend::Repl),
        UiMode::Tui if terminal => Ok(Frontend::Tui),
        UiMode::Tui => Err(
            "--ui tui requires terminal stdin and stdout; use --ui repl for redirected input"
                .to_string(),
        ),
    }
}
