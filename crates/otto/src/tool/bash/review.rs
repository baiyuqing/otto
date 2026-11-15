//! Review facts and git/script targets bound into a bash approval version.

use super::*;
use otto_platform::hex::to_hex;

impl BashTool {
    /// Bounded read-only payload snapshots become part of the exact prepared
    /// version, so a changed script/build recipe invalidates authorization.
    pub(super) fn review_facts(&self, command: &str) -> serde_json::Value {
        let parsed = shell::parse(command);
        let (cwd, segments) = match self.review_command_target(&parsed) {
            Ok(target) => target,
            Err(()) => return json!({"operation_unavailable":true,"complex_shell":true,"files":[]}),
        };
        let mut paths = Vec::<String>::new();
        let workspace = Workspace::new(&self.workspace_root);
        for (index, word) in segments.iter().flat_map(|words| words.iter().enumerate()) {
            let name = Path::new(word)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let command_word = index == 0;
            match name {
                "make" | "gmake" if command_word => {
                    let recipe = ["GNUmakefile", "makefile", "Makefile"]
                        .into_iter()
                        .find(|path| {
                            workspace.as_ref().is_ok_and(|workspace| {
                                workspace.open(&cwd.join(path)).is_ok_and(|file| {
                                    file.metadata().is_ok_and(|metadata| metadata.is_file())
                                })
                            })
                        })
                        .unwrap_or("Makefile");
                    paths.push(recipe.into());
                }
                "cargo" if command_word => paths.extend(["Cargo.toml".into(), "build.rs".into()]),
                "npm" | "pnpm" | "yarn" | "bun" if command_word => {
                    paths.push("package.json".into())
                }
                "git" if command_word => paths.extend([".git/HEAD".into(), ".git/config".into()]),
                _ => {}
            }
            if (command_word && word.starts_with("./"))
                || ["sh", "py", "js", "mjs", "cjs", "rb", "pl"]
                    .iter()
                    .any(|extension| word.ends_with(&format!(".{extension}")))
            {
                paths.push(word.clone());
            }
        }
        paths.sort();
        paths.dedup();
        let required_facts_truncated = paths.len() > 12;
        let mut total = 0usize;
        let files: Vec<_> = paths
            .into_iter()
            .take(12)
            .map(|path| {
                let read = (|| -> std::io::Result<Vec<u8>> {
                    let file = workspace
                        .as_ref()
                        .map_err(|_| std::io::Error::other("workspace unavailable"))?
                        .open(&cwd.join(&path))?;
                    if !file.metadata()?.is_file() {
                        return Err(std::io::Error::other("not a regular file"));
                    }
                    let limit = (24 * 1024usize).saturating_sub(total).min(12 * 1024);
                    let mut bytes = Vec::new();
                    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
                    if bytes.len() > limit {
                        return Err(std::io::Error::other("payload exceeds limit"));
                    }
                    total += bytes.len();
                    Ok(bytes)
                })();
                match read {
                    Ok(bytes) => {
                        let body = String::from_utf8_lossy(&bytes);
                        match self.redact_review_fact(&body) {
                            Some(body) => json!({"path":path,"sha256":to_hex(&sha2::Sha256::digest(&bytes)),"body":body}),
                            None => json!({"path":path,"unavailable":true}),
                        }
                    }
                    Err(error) => json!({"path":path,"unavailable":true,"optional": path == "build.rs" && error.kind() == std::io::ErrorKind::NotFound}),
                }
            })
            .collect();
        let mut target_unavailable = parsed.write_targets.iter().any(|target| !target.is_static);
        let targets: Vec<_> = parsed.write_targets.iter()
            .map(|target| {
                let path = cwd.join(&target.path);
                let resolved = workspace.as_ref().ok().and_then(|workspace| workspace.resolve_for_write(&path).ok())
                    .or_else(|| path.canonicalize().ok())
                    .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)));
                match resolved {
                    Some(resolved) if target.is_static && !target.path.contains(['$', '`', '*', '?', '~']) => {
                        let metadata = std::fs::metadata(&resolved).ok();
                        json!({"path":target.path,"resolved":resolved,"exists":metadata.is_some(),
                            "regular_file":metadata.as_ref().is_some_and(|metadata| metadata.is_file()),
                            "outside_workspace":!resolved.starts_with(&self.workspace_root)})
                    },
                    _ => { target_unavailable = true; json!({"path":target.path,"unavailable":true}) },
                }
            }).collect();
        json!({"cwd":cwd,"cwd_semantics":"initial literal cwd; the complete shell program remains untrusted operation data",
            "files":files,"targets":targets,"required_facts_truncated":required_facts_truncated,"target_unavailable":target_unavailable,
            "complex_shell":!parsed.command.complete,
            "scope":"read-only workspace payload snapshots; unavailable or indirect effects require human review"})
    }

    pub(super) fn redact_review_fact(&self, body: &str) -> Option<String> {
        let mut secrets = self.redact_values.clone();
        for token in body
            .split_whitespace()
            .filter(|token| token.contains("://"))
        {
            let (values, incomplete) = otto_platform::urlprivacy::userinfo_forms(token.as_bytes());
            if incomplete {
                return None;
            }
            secrets.extend(values);
        }
        let marker = dynamic_redaction_marker(&secrets)?;
        Some(redact_exact_text(body, &secrets, &marker))
    }

    /// Directory and remaining commands for the review facts. A first
    /// `cd <dir>` sets the directory; any other `cd`, `pushd` or `popd`, or a
    /// `cd` whose arguments are not all static, makes the target unavailable.
    /// After a leading `cd`, static `NAME=value` prefixes on simple commands
    /// are accepted (`Parsed::complete_with_command_env`).
    fn review_command_target(
        &self,
        parsed: &shell::Parsed,
    ) -> Result<(PathBuf, Vec<Vec<String>>), ()> {
        let analysis = &parsed.command;
        let mut cwd = self.workspace_root.clone();
        let mut segments = analysis.segments.as_slice();
        let first = segments.first().ok_or(())?;
        if shell::is_program(&first[0], "cd") {
            if !parsed.complete_with_command_env || first.len() != 2 {
                return Err(());
            }
            cwd = cwd.join(&first[1]);
            segments = &segments[1..];
        }
        let words = segments.first().ok_or(())?;
        if segments.iter().any(|words| {
            ["cd", "pushd", "popd"]
                .iter()
                .any(|name| shell::is_program(&words[0], name))
        }) {
            return Err(());
        }
        let git = shell::is_program(&words[0], "git");
        if git || shell::is_program(&words[0], "make") || shell::is_program(&words[0], "gmake") {
            let end = if git {
                shell::git_subcommand(words).map_or(words.len(), |(index, _)| index)
            } else {
                words.len()
            };
            let mut index = 1;
            while index < end {
                if words[index] == "-C" {
                    cwd = cwd.join(words.get(index + 1).ok_or(())?);
                    index += 2;
                } else if let Some(path) = words[index].strip_prefix("-C") {
                    cwd = cwd.join(path);
                    index += 1;
                } else {
                    index += 1;
                }
            }
        }
        if cwd.to_string_lossy().contains(['$', '`']) {
            return Err(());
        }
        let workspace = Workspace::new(&self.workspace_root).map_err(|_| ())?;
        let cwd = workspace.resolve_existing(&cwd).map_err(|_| ())?;
        if !cwd.is_dir() {
            return Err(());
        }
        Ok((cwd, segments.to_vec()))
    }

    /// Resolve only literal git cwd syntax. Ambiguous shell/config overrides
    /// never borrow branch or diff facts from the original workspace.
    pub(super) fn git_review_target(&self, command: &str) -> Result<Option<PathBuf>, ()> {
        let parsed = shell::parse(command);
        if parsed.assigns_variables {
            return Err(());
        }
        let (cwd, segments) = self.review_command_target(&parsed)?;
        if !shell::is_program(&segments[0][0], "git") {
            return Ok(None);
        }
        if segments.iter().flatten().any(|arg| {
            arg == "-c" || arg.starts_with("--git-dir") || arg.starts_with("--work-tree")
        }) {
            return Err(());
        }
        Ok(Some(cwd))
    }

    /// First push has no upstream. Use only an existing remote-tracking base
    /// and a verified common ancestor; never fabricate an empty outgoing diff.
    pub(super) async fn first_push_diff(
        &self,
        target: &Path,
        command: &str,
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Option<(String, serde_json::Value)> {
        let analysis = shell::analyze(command);
        let (words, push) = analysis.segments.iter().find_map(|words| {
            let (index, subcommand) = shell::git_subcommand(words)?;
            (subcommand == "push").then_some((words, index))
        })?;
        let remote = words[push + 1..]
            .iter()
            .find(|word| !word.starts_with('-'))?;
        if !remote
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return None;
        }
        for name in ["HEAD", "main", "master"] {
            let reference = format!("refs/remotes/{remote}/{name}");
            let commit_ref = format!("{reference}^{{commit}}");
            let Some(base) = crate::sandbox::git::run_git(
                &self.executor,
                &target.to_string_lossy(),
                &self.environment,
                &["rev-parse", "--verify", &commit_ref],
                deadline,
                cancel,
            )
            .await
            else {
                continue;
            };
            let base = base.trim();
            if !matches!(base.len(), 40 | 64) || !base.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                continue;
            }
            let Some(ancestor) = crate::sandbox::git::run_git(
                &self.executor,
                &target.to_string_lossy(),
                &self.environment,
                &["merge-base", base, "HEAD"],
                deadline,
                cancel,
            )
            .await
            else {
                continue;
            };
            let ancestor = ancestor.trim();
            if !matches!(ancestor.len(), 40 | 64)
                || !ancestor.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                continue;
            }
            let Some(diff) = crate::sandbox::git::run_git(
                &self.executor,
                &target.to_string_lossy(),
                &self.environment,
                &[
                    "--no-pager",
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    ancestor,
                    "HEAD",
                ],
                deadline,
                cancel,
            )
            .await
            else {
                continue;
            };
            return Some((
                self.redact_review_fact(&diff)?,
                json!({"reference":reference,"commit":base,"merge_base":ancestor}),
            ));
        }
        None
    }
}
