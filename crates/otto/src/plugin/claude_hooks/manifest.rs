//! Discovery of Claude-format hooks in a `.claude-plugin` package: `hooks/hooks.json` and the
//! manifest `hooks` field. Every skipped handler or group yields exactly one diagnostic.

use super::{
    ClaudeHook, MAX_TIMEOUT_SECONDS, default_timeout_seconds, matcher::Matcher, otto_event,
};
use crate::fsops::root::Root;
use serde_json::{Map, Value};
use std::path::{Component, Path};

/// Hooks found in a package and the diagnostics for everything that was skipped or degraded.
#[derive(Debug, Default)]
pub struct Discovered {
    pub hooks: Vec<ClaudeHook>,
    pub diagnostics: Vec<String>,
}

const HOOKS_FILE: &str = "hooks/hooks.json";
const PLUGIN_ROOT_PREFIX: &str = "${CLAUDE_PLUGIN_ROOT}/";

/// Reads `hooks/hooks.json` when present, then the manifest `hooks` field. `manifest` is the
/// parsed `.claude-plugin/plugin.json`.
pub fn discover(root: &Root, manifest: &Value) -> Discovered {
    let mut found = Discovered::default();
    if root.lstat(Path::new(HOOKS_FILE)).is_ok() {
        match read_wrapped_file(root, HOOKS_FILE) {
            Ok(config) => parse_into(&config, HOOKS_FILE, &mut found),
            Err(reason) => found
                .diagnostics
                .push(format!("{HOOKS_FILE}: invalid {HOOKS_FILE}: {reason}")),
        }
    }
    match manifest.get("hooks") {
        None => {}
        Some(Value::Array(entries)) => {
            for entry in entries {
                discover_manifest_entry(root, entry, &mut found);
            }
        }
        Some(entry) => discover_manifest_entry(root, entry, &mut found),
    }
    found
}

/// Parses one hook configuration object without the `hooks` wrapper.
pub fn parse(config: &Value, source: &str) -> Discovered {
    let mut found = Discovered::default();
    parse_into(config, source, &mut found);
    found
}

/// Returns the `hooks` object of a wrapped file, or why the file is not one.
fn read_wrapped_file(root: &Root, path: &str) -> Result<Value, String> {
    let bytes = super::super::package::read(root, Path::new(path))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    wrapped_hooks(&value)
}

fn wrapped_hooks(value: &Value) -> Result<Value, String> {
    let object = value.as_object().ok_or("top level must be an object")?;
    match object.get("hooks") {
        Some(hooks) if hooks.is_object() => Ok(hooks.clone()),
        _ => Err("missing `hooks` object".into()),
    }
}

fn discover_manifest_entry(root: &Root, entry: &Value, found: &mut Discovered) {
    const SOURCE: &str = "manifest hooks";
    match entry {
        Value::String(raw_path) => {
            let Some(path) = normalize_hooks_path(raw_path) else {
                found
                    .diagnostics
                    .push(format!("{SOURCE}: invalid hooks path {raw_path}"));
                return;
            };
            match super::super::package::read(root, Path::new(&path)) {
                Err(reason) => found.diagnostics.push(format!(
                    "{SOURCE}: cannot read hooks file {raw_path}: {reason}"
                )),
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Err(error) => found
                        .diagnostics
                        .push(format!("{SOURCE}: invalid hooks file {raw_path}: {error}")),
                    Ok(value) => match wrapped_hooks(&value) {
                        Ok(config) => parse_into(&config, &path, found),
                        Err(reason) => found
                            .diagnostics
                            .push(format!("{SOURCE}: invalid hooks file {raw_path}: {reason}")),
                    },
                },
            }
        }
        Value::Object(object) => {
            if is_wrapper_only(object) {
                parse_into(&object["hooks"], SOURCE, found);
            } else {
                parse_into(entry, SOURCE, found);
            }
        }
        _ => found
            .diagnostics
            .push(format!("{SOURCE}: invalid manifest hooks field")),
    }
}

/// An object whose only keys are `hooks` (an object) and optionally `description`.
fn is_wrapper_only(object: &Map<String, Value>) -> bool {
    object.get("hooks").is_some_and(Value::is_object)
        && object
            .keys()
            .all(|key| key == "hooks" || key == "description")
}

/// Strips `${CLAUDE_PLUGIN_ROOT}/` and `./`; `None` for an empty, absolute or `..` path.
fn normalize_hooks_path(raw_path: &str) -> Option<String> {
    let stripped = raw_path
        .strip_prefix(PLUGIN_ROOT_PREFIX)
        .unwrap_or(raw_path);
    let stripped = stripped.strip_prefix("./").unwrap_or(stripped);
    let path = Path::new(stripped);
    let escapes = path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if stripped.is_empty() || path.is_absolute() || escapes {
        return None;
    }
    Some(stripped.to_owned())
}

fn parse_into(config: &Value, source: &str, found: &mut Discovered) {
    let Some(events) = config.as_object() else {
        found
            .diagnostics
            .push(format!("{source}: invalid hook configuration"));
        return;
    };
    for (event, groups) in events {
        if otto_event(event).is_none() {
            found
                .diagnostics
                .push(format!("{source}: hook event {event} never fires in Otto"));
            continue;
        }
        let Some(group_list) = groups
            .as_array()
            .filter(|list| list.iter().all(Value::is_object))
        else {
            found
                .diagnostics
                .push(format!("{source}: invalid hook groups for {event}"));
            continue;
        };
        for group in group_list {
            parse_group(group, event, source, found);
        }
    }
}

fn parse_group(group: &Value, event: &str, source: &str, found: &mut Discovered) {
    let matcher_text = match group.get("matcher") {
        None => "",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => {
            found
                .diagnostics
                .push(format!("{source}: invalid hook matcher for {event}"));
            return;
        }
    };
    let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
        found
            .diagnostics
            .push(format!("{source}: invalid hook group for {event}"));
        return;
    };
    for handler in handlers {
        match parse_handler(handler, event, matcher_text) {
            Ok((hook, notice)) => {
                found
                    .diagnostics
                    .extend(notice.map(|text| format!("{source}: {text}")));
                if !found
                    .hooks
                    .iter()
                    .any(|existing| same_handler(existing, &hook))
                {
                    found.hooks.push(hook);
                }
            }
            Err(message) => found.diagnostics.push(format!("{source}: {message}")),
        }
    }
}

fn same_handler(left: &ClaudeHook, right: &ClaudeHook) -> bool {
    left.event == right.event
        && left.matcher == right.matcher
        && left.command == right.command
        && left.args == right.args
}

/// Builds one hook from a handler object. `Ok` carries an optional notice for a handler that
/// is kept; `Err` carries the diagnostic for a skipped handler.
fn parse_handler(
    handler: &Value,
    event: &str,
    matcher_text: &str,
) -> Result<(ClaudeHook, Option<String>), String> {
    let Some(handler_object) = handler.as_object() else {
        return Err(format!("invalid hook handler for {event}"));
    };
    match handler_object.get("type") {
        None => return Err(format!("missing hook type for {event}")),
        Some(Value::String(kind)) if kind == "command" => {}
        Some(Value::String(kind)) => {
            return Err(format!("unsupported hook type {kind} for {event}"));
        }
        Some(other) => return Err(format!("unsupported hook type {other} for {event}")),
    }
    if handler_object.contains_key("if") {
        return Err(format!("unsupported hook condition for {event}"));
    }
    let shell = match handler_object.get("shell") {
        None => super::default_shell(),
        Some(Value::String(shell)) if shell == "sh" || shell == "bash" => shell.clone(),
        Some(Value::String(shell)) => {
            return Err(format!("unsupported hook shell {shell} for {event}"));
        }
        Some(other) => return Err(format!("unsupported hook shell {other} for {event}")),
    };
    let command = match handler_object.get("command").and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() && !text.contains('\0') => text.to_owned(),
        _ => return Err(format!("invalid hook command for {event}")),
    };
    let args = parse_args(handler_object.get("args"), event)?;
    let timeout_seconds = parse_timeout(handler_object.get("timeout"), event)?;
    let matcher = if matcher_text == "*" {
        ""
    } else {
        matcher_text
    };
    if Matcher::parse(matcher).is_err() {
        return Err(format!("invalid hook matcher {matcher_text} for {event}"));
    }
    let notice = (handler_object.get("async") == Some(&Value::Bool(true)))
        .then(|| format!("async hook for {event} runs synchronously"));
    let hook = ClaudeHook {
        event: event.to_owned(),
        matcher: matcher.to_owned(),
        command,
        args,
        shell,
        timeout_seconds,
    };
    Ok((hook, notice))
}

fn parse_args(args: Option<&Value>, event: &str) -> Result<Option<Vec<String>>, String> {
    let Some(value) = args else { return Ok(None) };
    let invalid = || format!("invalid hook args for {event}");
    let list = value.as_array().ok_or_else(invalid)?;
    let mut strings = Vec::with_capacity(list.len());
    for item in list {
        match item.as_str() {
            Some(text) if !text.contains('\0') => strings.push(text.to_owned()),
            _ => return Err(invalid()),
        }
    }
    Ok(Some(strings))
}

fn parse_timeout(timeout: Option<&Value>, event: &str) -> Result<u64, String> {
    let Some(value) = timeout else {
        return Ok(default_timeout_seconds(event));
    };
    match value.as_f64() {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Ok((seconds.ceil() as u64).min(MAX_TIMEOUT_SECONDS))
        }
        _ => Err(format!("invalid hook timeout for {event}")),
    }
}
