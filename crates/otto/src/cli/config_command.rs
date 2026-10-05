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
    let loaded = crate::config::layered::load(&home, &user, explicit, &cwd)?;
    match args[0].as_str() {
        "explain" if words.is_empty() && scope.is_none() => {
            let public_env = HashMap::from([("HOME".to_string(), home.display().to_string())]);
            let effective = otto_core::config::resolve::resolve_without_credentials(&loaded.file, &public_env, &Default::default(), &Default::default()).ok().map(|r| serde_json::json!({"profile":r.profile,"provider":r.provider,"model":r.model,"thinking":r.thinking,"max_output_bytes":r.max_output_bytes,"shell_timeout_seconds":r.shell_timeout.as_secs(),"compaction":{"auto":r.compaction.auto,"reserve_tokens":r.compaction.reserve_tokens,"keep_recent_tokens":r.compaction.keep_recent_tokens}}));
            let report = serde_json::json!({"workspace_boundary":loaded.boundary, "project_sources":loaded.sources, "project_executables_trusted":loaded.trusted, "configuration":loaded.merged.explained(), "managed_policy": loaded.file.policy.as_ref().map(|p| serde_json::json!({"require_sandbox":p.require_sandbox,"allow_project_executables":p.allow_project_executables,"ceilings":p.ceilings})), "defaults":"Unspecified values use built-in runtime defaults; environment credentials are never resolved by explain."});
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
                crate::config::layered::project_read(&owner, relative)?
                    .unwrap_or_default()
                    .into_bytes()
            };
            let text = std::str::from_utf8(&old).map_err(|_| "configuration is not UTF-8")?;
            let keys: Vec<_> = words[0].split('.').collect();
            if keys.iter().any(|k| k.is_empty()) {
                return Err("invalid configuration key".into());
            }
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
                otto_core::config::edit::insert_table(text, table, body)
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
            crate::config::write_bytes(&target, &old, updated.as_bytes())
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
}
