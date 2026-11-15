//! Package-only Lua admission and cancellable execution.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn validate(
    command: &[String],
    lua: Option<&str>,
    timeout: &str,
) -> Result<Duration, String> {
    match lua {
        None => validate_command(command, timeout),
        Some(path) => {
            if !command.is_empty() {
                return Err("choose exactly one command or lua handler".into());
            }
            let path = Path::new(path);
            if path.as_os_str().is_empty()
                || path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                || path.extension().is_none_or(|ext| ext != "lua")
            {
                return Err(
                    "lua handler requires a package-relative .lua source path without traversal"
                        .into(),
                );
            }
            validate_command(&["lua".into()], timeout)
        }
    }
}
pub(super) fn source(root: &Path, path: &str) -> Result<Vec<u8>, String> {
    validate(&[], Some(path), "")?;
    let root = crate::fsops::root::Root::open(root).map_err(|_| "cannot open Lua package")?;
    let file = root
        .open_file(
            Path::new(path),
            nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|_| "cannot read package Lua source")?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err("Lua source must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(super::lua_engine::MAX_SOURCE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read Lua source")?;
    if bytes.len() as u64 > super::lua_engine::MAX_SOURCE
        || bytes.starts_with(b"\x1bLua")
        || std::str::from_utf8(&bytes).is_err()
    {
        return Err("Lua source must be UTF-8 text at most 256 KiB; bytecode is forbidden".into());
    }
    Ok(bytes)
}
pub(crate) fn validate_sources(root: &Path, components: &Components) -> Result<(), String> {
    for path in components
        .hooks
        .entries()
        .iter()
        .flat_map(|(_, hooks)| hooks.iter().filter_map(|hook| hook.lua.as_deref()))
        .chain(
            components
                .tools
                .iter()
                .filter_map(|tool| tool.lua.as_deref()),
        )
    {
        source(root, path)?;
    }
    Ok(())
}
struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
impl Runtime {
    pub(super) async fn lua(
        &self,
        path: &str,
        timeout: &str,
        package: Option<&Package>,
        data: Value,
        control: &dyn OperationControl,
        guard: Option<&Arc<SessionGuard>>,
    ) -> Result<String, CommandError> {
        let package = package.ok_or("Lua handlers require an installed trusted package")?;
        let event = data.get("event").and_then(Value::as_str).unwrap_or("tool");
        if control.admission_stop_reason().is_some()
            || guard.is_some_and(|guard| guard.before(event).is_err())
        {
            return Err("Lua operation stopped or session ownership lost".into());
        }
        if !self.valid(Some(package)) {
            return Err("plugin source changed or trust revoked; review and restart".into());
        }
        let source = source(&package.root, path).map_err(CommandError::from)?;
        if !self.valid(Some(package)) {
            return Err("plugin source changed while reading Lua source".into());
        }
        let input = self.redacted_input(&data)?;
        let timeout = validate(&[], Some(path), timeout)
            .map_err(CommandError::from)?
            .min(control.remaining().unwrap_or(Duration::from_secs(300)));
        let invocation_deadline = std::time::Instant::now() + timeout;
        let stopped = Arc::new(AtomicBool::new(false));
        let _stop = StopOnDrop(stopped.clone());
        let mut job = tokio::task::spawn_blocking({
            let stopped = stopped.clone();
            #[cfg(test)]
            let observer = self.lua_observer.lock().unwrap().clone();
            move || {
                #[cfg(test)]
                if let Some(observer) = observer {
                    let result = super::lua_engine::run_observed(
                        &source,
                        &input,
                        invocation_deadline,
                        stopped,
                        observer.clone(),
                    );
                    // run returned: all VM-owned values and the VM have been destroyed.
                    observer.succeeded.store(result.is_ok(), Ordering::Release);
                    observer.exited.notify_one();
                    return result;
                }
                super::lua_engine::run(&source, &input, invocation_deadline, stopped)
            }
        });
        #[cfg(test)]
        {
            let observer = self.lua_observer.lock().unwrap().clone();
            if let Some(observer) = observer.filter(|observer| observer.hold_delivery) {
                observer.delivery.notified().await;
            }
        }
        let deadline = tokio::time::sleep_until(invocation_deadline.into());
        tokio::pin!(deadline);
        let mut poll = tokio::time::interval(Duration::from_millis(10));
        let failure = loop {
            tokio::select! {
                biased;
                result = &mut job => {
                    let result = result.map_err(|_| CommandError::started("Lua worker failed"))?
                        .map_err(|error| CommandError::started(&error))?;
                    if !self.valid(Some(package)) || guard.is_some_and(|guard| guard.after(event).is_err()) {
                        return Err(CommandError::started("plugin trust or session ownership changed during Lua execution"));
                    }
                    if control.stop_reason().is_some() { return Err(CommandError::interrupted("Lua execution cancelled")); }
                    if std::time::Instant::now() >= invocation_deadline {
                        return Err(CommandError::interrupted("Lua execution timed out"));
                    }
                    return Ok(result);
                }
                _ = control.cancellation_token().cancelled() => break "Lua execution cancelled",
                _ = &mut deadline => break "Lua execution timed out",
                _ = poll.tick() => {
                    if !self.valid(Some(package)) || guard.is_some_and(|guard| guard.before(event).is_err()) {
                        break "plugin trust or session ownership revoked during Lua execution";
                    }
                }
            }
        };
        stopped.store(true, Ordering::Relaxed);
        #[cfg(test)]
        if let Some(observer) = self.lua_observer.lock().unwrap().as_ref() {
            observer.stopping.notify_one();
        }
        // Join after signalling: instruction hooks terminate CPU loops; no detached unbounded worker.
        let _ = job.await;
        Err(CommandError::interrupted(failure))
    }
}
