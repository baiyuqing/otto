//! Configuration inspection and explicit-scope editing; no provider or MCP startup.
use super::run::{fail, resolve_home_for};
use std::{collections::HashMap, io::Write, path::PathBuf};

const USAGE: &str = "usage: otto config explain [--cwd PATH] [--config PATH] [--json]\n       otto config set KEY TOML_VALUE --scope user|shared|local [--cwd PATH] [--config PATH]";

pub fn run(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    match execute(args, stdout, lookup) {
        Ok(()) => 0,
        Err(e) => fail(stderr, &e),
    }
}

fn execute(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> Result<(), String> {
    if args.is_empty() || args.iter().any(|a| a == "--help") {
        writeln!(stdout, "{USAGE}").map_err(|_| "cannot write help")?;
        return Ok(());
    }
    let mut cwd = PathBuf::from(".");
    let mut user = None;
    let mut scope = None;
    let mut json = false;
    let mut words = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cwd" | "--config" | "--scope" => {
                let name = &args[i];
                i += 1;
                let value = args.get(i).ok_or(USAGE)?;
                match name.as_str() {
                    "--cwd" => cwd = value.into(),
                    "--config" => user = Some(PathBuf::from(value)),
                    _ => scope = Some(value.as_str()),
                }
            }
            "--json" => json = true,
            a if a.starts_with('-') => return Err(USAGE.into()),
            _ => words.push(args[i].as_str()),
        }
        i += 1;
    }
    let home = PathBuf::from(resolve_home_for(lookup)?);
    let explicit = user.is_some();
    let user = user.unwrap_or_else(|| home.join(".config/otto/config.toml"));
    let loaded = otto::config::layered::load(&home, &user, explicit, &cwd)?;
    match args[0].as_str() {
        "explain" if words.is_empty() && scope.is_none() => {
            let mut public_env = HashMap::from([("HOME".to_string(), home.display().to_string())]);
            // Only selection inputs, never credential bindings or arbitrary environment values.
            for key in ["OTTO_PROFILE", "OTTO_PROVIDER", "OTTO_MODEL"] {
                if let Some(value) = lookup.get(key) {
                    public_env.insert(key.to_string(), value.clone());
                }
            }
            let effective = otto_core::config::resolve::resolve_without_credentials(
                &loaded.file,
                &public_env,
                &Default::default(),
                &Default::default(),
            )
            .ok()
            .map(|r| {
                let diagnostics: Vec<_> =
                    otto_core::config::resolve::subagent_model_diagnostic(&loaded.file, &r)
                        .into_iter()
                        .collect();
                serde_json::json!({"model":r.model_selector.to_string(),
                    "provider":r.provider_name,
                    "provider_type":r.provider_type,
                    "model_id":r.model,
                    "thinking":r.thinking,
                    "subagent_models":r.subagent_models,
                    "diagnostics":diagnostics,
                    "max_output_bytes":r.max_output_bytes,
                    "shell_timeout_seconds":r.shell_timeout.as_secs(),
                    "compaction":{"auto":r.compaction.auto,
                    "reserve_tokens":r.compaction.reserve_tokens,
                    "keep_recent_tokens":r.compaction.keep_recent_tokens}})
            });
            let report = serde_json::json!({"workspace_boundary":loaded.boundary,
                "project_sources":loaded.sources,
                "project_executables_trusted":loaded.trusted,
                "configuration":loaded.merged.explained(),
                "managed_policy": loaded.file.policy.as_ref().map(|p| serde_json::json!({
                    "require_sandbox":p.require_sandbox,
                    "allow_project_executables":p.allow_project_executables,
                    "ceilings":p.ceilings})),
                "defaults":"Unspecified values use built-in runtime defaults; environment \
                credentials are never resolved by explain."});
            let mut report = report;
            report["effective_runtime"] = effective.unwrap_or(serde_json::Value::Null);
            report["effective_runtime_note"] = "null means a complete provider/model has not been configured; raw merged values and provenance are still available".into();
            let text = if json {
                serde_json::to_string(&report)
            } else {
                serde_json::to_string_pretty(&report)
            }
            .map_err(|_| "cannot encode configuration report")?;
            writeln!(stdout, "{text}").map_err(|_| "cannot write configuration report")?;
        }
        "set" if words.len() == 2 && !json => {
            let target = match scope.ok_or("config set requires --scope user|shared|local")? {
                "user" => user,
                "shared" => std::fs::canonicalize(&cwd)
                    .map_err(|_| "invalid workspace")?
                    .join(".otto/config.toml"),
                "local" => std::fs::canonicalize(&cwd)
                    .map_err(|_| "invalid workspace")?
                    .join(".otto/config.local.toml"),
                _ => return Err(USAGE.into()),
            };
            let old = if scope == Some("user") {
                match std::fs::read(&target) {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(_) => return Err("cannot read target configuration".into()),
                }
            } else {
                let owner = std::fs::canonicalize(&cwd).map_err(|_| "invalid workspace")?;
                let relative = target
                    .strip_prefix(&owner)
                    .map_err(|_| "invalid project configuration path")?
                    .to_str()
                    .ok_or("invalid configuration path")?;
                otto::config::layered::project_read(&owner, relative)?
                    .unwrap_or_default()
                    .into_bytes()
            };
            let text = std::str::from_utf8(&old).map_err(|_| "configuration is not UTF-8")?;
            let owned_keys = split_config_key(words[0]).ok_or("invalid configuration key")?;
            let keys: Vec<&str> = owned_keys.iter().map(String::as_str).collect();
            let table = &keys[..keys.len() - 1];
            let key = keys[keys.len() - 1];
            let raw: toml::Table =
                toml::from_str(text).map_err(|_| "invalid target configuration")?;
            let mut existing = Some(&raw);
            for part in table {
                existing = existing
                    .and_then(|t| t.get(*part))
                    .and_then(toml::Value::as_table);
            }
            let updated = if existing.is_some() {
                otto_core::config::edit::set_value(text, table, key, Some(words[1]))
            } else {
                let body: toml::Table = toml::from_str(&format!("{key} = {}", words[1]))
                    .map_err(|_| "invalid configuration value")?;
                match table {
                    ["providers", provider, "models", model_id] => {
                        otto::config::add_model_entry(text, provider, model_id, body)
                    }
                    _ => otto_core::config::edit::insert_table(text, table, body),
                }
            }
            .map_err(|_| "invalid configuration edit")?;
            otto_core::config::parse(&updated).map_err(|_| "configuration edit is invalid")?;
            if target.starts_with(std::fs::canonicalize(&cwd).map_err(|_| "invalid workspace")?)
                && scope != Some("user")
            {
                let mut check = otto_core::config::layered::Merged::default();
                check
                    .add(
                        &updated,
                        &target.display().to_string(),
                        otto_core::config::layered::Layer::Local,
                        true,
                    )
                    .map_err(|e| e.to_string())?;
                if !check.diagnostics.is_empty() {
                    return Err(
                        "requested fields are not permitted in project configuration".into(),
                    );
                }
                // Pin the owning directory; never follow a project .otto escape when writing.
                if target.parent().is_some_and(|p| {
                    p.exists()
                        && std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
                }) {
                    return Err("unsafe project configuration directory".into());
                }
            }
            otto::config::write_bytes(&target, &old, updated.as_bytes())
                .map_err(|_| "cannot save configuration")?;
            writeln!(
                stdout,
                "Updated {}. Restart affected workspace runtimes to reload.",
                target.display()
            )
            .map_err(|_| "cannot write edit result")?;
            if scope == Some("local") {
                writeln!(
                    stdout,
                    "Add .otto/config.local.toml to repository ignore rules."
                )
                .map_err(|_| "cannot write edit result")?;
            }
            if loaded
                .merged
                .origins
                .get(words[0])
                .is_some_and(|origin| origin.source != target.display().to_string())
            {
                writeln!(
                    stdout,
                    "Another configuration source may shadow this value; run config explain."
                )
                .map_err(|_| "cannot write edit result")?;
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// Splits a dotted configuration key such as
/// `providers.chatgpt.models."gpt-5.6-luna".thinking` into its parts, decoding
/// quoted parts the way a TOML table header does. `None` when the key is not
/// valid TOML key syntax, has an empty part, or spans several lines.
pub(crate) fn split_config_key(key: &str) -> Option<Vec<String>> {
    if key.contains(['\n', '\r']) {
        return None;
    }
    let mut table: toml::Table = toml::from_str(&format!("[{key}]")).ok()?;
    let mut parts = Vec::new();
    while !table.is_empty() {
        let (part, value) = table.into_iter().next()?;
        let toml::Value::Table(inner) = value else {
            return None;
        };
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        table = inner;
    }
    (!parts.is_empty()).then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explain_is_offline_and_does_not_expose_bindings() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/otto");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "[mcp.servers.a]\ntransport='http'\nurl='https://example.test'\nheaders={Authorization='secret-marker'}").unwrap();
        let args = vec![
            "explain".into(),
            "--json".into(),
            "--cwd".into(),
            workspace.path().display().to_string(),
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(
                &args,
                &mut out,
                &mut err,
                &HashMap::from([("HOME".into(), home.path().display().to_string())])
            ),
            0
        );
        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("secret-marker"));
        assert!(output.contains("redacted"));
    }
    #[test]
    fn explain_reports_the_selected_model_without_credentials_or_legacy_values() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/otto");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            concat!(
                "default_profile='work'\n[profiles.work]\nprovider='openai-compatible'\n",
                "model='parent'\nbase_url='https://example.test/v1'\n",
                "api_key_env='PRIVATE_KEY'\nsubagent_models=['child']\n"
            ),
        )
        .unwrap();
        let args = [
            "explain",
            "--json",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ]
        .map(str::to_string);
        let lookup = HashMap::from([
            ("HOME".into(), home.path().display().to_string()),
            ("OTTO_MODEL".into(), "chatgpt/other-model".into()),
            ("PRIVATE_KEY".into(), "credential-marker".into()),
            ("UNRELATED".into(), "unrelated-marker".into()),
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(run(&args, &mut out, &mut err, &lookup), 0);
        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("credential-marker"));
        assert!(!output.contains("unrelated-marker"));
        let report: serde_json::Value = serde_json::from_str(&output).unwrap();
        let runtime = &report["effective_runtime"];
        assert_eq!(runtime["provider"], "chatgpt");
        assert_eq!(runtime["subagent_models"], serde_json::json!([]));
        assert_eq!(runtime["model"], "chatgpt/other-model");
        assert_eq!(runtime["provider_type"], "chatgpt");
        assert_eq!(runtime["model_id"], "other-model");
        // The explicit selector names a provider that has no legacy child models,
        // so no fallback diagnostic is reported and no configured value is printed.
        assert_eq!(runtime["diagnostics"], serde_json::json!([]));
        for private in [
            "PRIVATE_KEY",
            "example.test",
            "child\"",
            "credential-marker",
        ] {
            assert!(
                !runtime.to_string().contains(private),
                "{private} leaked into {runtime}"
            );
        }
    }

    #[test]
    fn writing_local_configuration_leaves_global_untouched() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "set".into(),
            "ui.mode".into(),
            "'repl'".into(),
            "--scope".into(),
            "local".into(),
            "--cwd".into(),
            workspace.path().display().to_string(),
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(
                &args,
                &mut out,
                &mut err,
                &HashMap::from([("HOME".into(), home.path().display().to_string())])
            ),
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        assert!(workspace.path().join(".otto/config.local.toml").exists());
        assert!(!home.path().join(".config/otto/config.toml").exists());
    }
    #[test]
    fn explicit_scope_edit_preserves_comments_and_rejects_project_authority() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(".otto")).unwrap();
        let path = project.path().join(".otto/config.toml");
        std::fs::write(
            &path,
            "# team preferences\n[ui]\nmode = 'auto' # keep this\n",
        )
        .unwrap();
        let env = HashMap::from([("HOME".into(), home.path().display().to_string())]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = [
            "set",
            "ui.mode",
            "'repl'",
            "--scope",
            "shared",
            "--cwd",
            project.path().to_str().unwrap(),
        ]
        .map(str::to_string);
        assert_eq!(run(&args, &mut out, &mut err, &env), 0);
        let expected = std::fs::read_to_string(&path).unwrap();
        assert!(expected.contains("# team preferences"));
        assert!(expected.contains("# keep this"));
        assert!(path.parent().unwrap().join("backups").exists());
        let args = [
            "set",
            "sandbox.driver",
            "'off'",
            "--scope",
            "shared",
            "--cwd",
            project.path().to_str().unwrap(),
        ]
        .map(str::to_string);
        assert_eq!(run(&args, &mut out, &mut err, &env), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }
    #[test]
    fn set_writes_a_new_model_key_as_an_inline_entry_with_a_quoted_model_id() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/otto");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[providers.chatgpt]\ntype = 'chatgpt'\n").unwrap();
        let env = HashMap::from([("HOME".into(), home.path().display().to_string())]);
        let args = [
            "set",
            "providers.chatgpt.models.\"gpt-5.6-luna\".thinking",
            "'high'",
            "--scope",
            "user",
        ]
        .map(str::to_string);
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(&args, &mut out, &mut err, &env),
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[providers.chatgpt]\ntype = 'chatgpt'\nmodels.\"gpt-5.6-luna\" = { thinking = \"high\" }\n"
        );
    }
    #[test]
    fn set_keeps_header_style_models_of_a_provider_in_header_style() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/otto");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[providers.chatgpt.models.\"a\"]\nthinking = 'low'\n",
        )
        .unwrap();
        let env = HashMap::from([("HOME".into(), home.path().display().to_string())]);
        let args = [
            "set",
            "providers.chatgpt.models.\"b\".thinking",
            "\"high\"",
            "--scope",
            "user",
        ]
        .map(str::to_string);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run(&args, &mut out, &mut err, &env),
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[providers.chatgpt.models.b]\nthinking = \"high\""),
            "{text}"
        );
        assert!(!text.contains("[providers.chatgpt]"), "{text}");
    }
    #[test]
    fn config_keys_split_at_dots_outside_quotes() {
        assert_eq!(
            split_config_key("providers.chatgpt.models.\"gpt-5.6-luna\".thinking").unwrap(),
            ["providers", "chatgpt", "models", "gpt-5.6-luna", "thinking"]
        );
        assert_eq!(split_config_key("ui.mode").unwrap(), ["ui", "mode"]);
        assert!(split_config_key("a..b").is_none());
        assert!(split_config_key("a\n[b").is_none());
        assert!(split_config_key("").is_none());
    }
}
