//! Native discovery and executable trust. Never writes an effective configuration.
use crate::tool::workspace::Workspace;
use otto_core::config::{
    File,
    layered::{Layer, ManagedPolicy, Merged},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MAX_CONFIG: u64 = 1024 * 1024;

pub struct Loaded {
    pub file: File,
    pub merged: Merged,
    pub boundary: PathBuf,
    pub sources: Vec<PathBuf>,
    pub trusted: bool,
}

pub fn managed_path() -> &'static Path {
    #[cfg(target_os = "macos")]
    let path = "/Library/Application Support/Otto/policy.toml";
    #[cfg(not(target_os = "macos"))]
    let path = "/etc/otto/policy.toml";
    Path::new(path)
}

pub fn policy_at(path: &Path) -> Result<Option<ManagedPolicy>, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot inspect managed policy".into()),
        Ok(_) => {}
    }
    for ancestor in path.ancestors() {
        let m = std::fs::symlink_metadata(ancestor)
            .map_err(|_| "cannot inspect managed policy ownership")?;
        if m.file_type().is_symlink() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err("managed policy must be administrator-owned and not user-writable".into());
        }
    }
    let mut file = crate::fsops::open_no_follow(path, libc::O_RDONLY)
        .map_err(|_| "cannot read managed policy")?;
    let text = bounded_read(&mut file)?;
    toml::from_str(&text)
        .map(Some)
        .map_err(|_| "invalid managed policy".into())
}

fn bounded_read(file: &mut std::fs::File) -> Result<String, String> {
    if !file
        .metadata()
        .map_err(|_| "cannot inspect configuration")?
        .is_file()
    {
        return Err("configuration is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read configuration")?;
    if bytes.len() as u64 > MAX_CONFIG {
        return Err("configuration exceeds 1 MiB".into());
    }
    String::from_utf8(bytes).map_err(|_| "configuration is not UTF-8".into())
}

/// A bounded UTF-8 source read through a pinned owning directory, also used by scoped edits.
pub fn project_read(dir: &Path, relative: &str) -> Result<Option<String>, String> {
    let path = dir.join(relative);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("cannot inspect {}", path.display())),
        Ok(_) => {}
    }
    let workspace = Workspace::new(dir).map_err(|_| "invalid configuration boundary")?;
    let mut file = workspace
        .open(Path::new(relative))
        .map_err(|_| format!("unsafe configuration path: {}", path.display()))?;
    bounded_read(&mut file).map(Some)
}

fn git_boundary(dir: &Path) -> Result<bool, String> {
    let marker = dir.join(".git");
    let info = match std::fs::symlink_metadata(&marker) {
        Ok(info) => info,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("cannot inspect project boundary".into()),
    };
    if info.file_type().is_symlink() {
        return Err("project Git marker must not be a symlink".into());
    }
    if info.is_dir() {
        // An empty directory is not a Git repository (managed environments may
        // reserve .git this way). Real repositories have a HEAD entry.
        return std::fs::symlink_metadata(marker.join("HEAD"))
            .map(|m| m.is_file())
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(false)
                } else {
                    Err(e)
                }
            })
            .map_err(|_| "cannot inspect Git HEAD".into());
    }
    if info.is_file() {
        return Ok(project_read(dir, ".git")?.is_some_and(|text| {
            text.trim()
                .strip_prefix("gitdir:")
                .is_some_and(|path| !path.trim().is_empty())
        }));
    }
    Ok(false)
}

pub fn directories(workspace: &Path) -> Result<Vec<PathBuf>, String> {
    let workspace =
        std::fs::canonicalize(workspace).map_err(|_| "workspace directory is unavailable")?;
    if !workspace.is_dir() {
        return Err("workspace is not a directory".into());
    }
    let mut chain = Vec::new();
    for dir in workspace.ancestors() {
        chain.push(dir.to_path_buf());
        if git_boundary(dir)? {
            chain.reverse();
            return Ok(chain);
        }
    }
    // No repository: do not inherit arbitrary parent directories.
    Ok(vec![workspace])
}

/// Management commands read and edit the owning user source, never a merged project.
pub fn load_user(_home: &Path, user: &Path, explicit: bool) -> Result<File, String> {
    let mut merged = Merged::default();
    match std::fs::read_to_string(user) {
        Ok(text) => merged
            .add(&text, &user.display().to_string(), Layer::User, true)
            .map_err(|e| e.to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {}
        Err(_) => return Err("user configuration is unavailable".into()),
    }
    let snapshot = merged.values.clone();
    let policy = policy_at(managed_path())?;
    if let Some(p) = &policy {
        p.apply(&mut merged, &managed_path().display().to_string())
            .map_err(|e| e.to_string())?;
    }
    let mut file = merged.file().map_err(|e| e.to_string())?;
    file.user_values = Some(snapshot);
    file.user_source = Some(user.display().to_string());
    file.policy = policy;
    Ok(file)
}

pub fn load(home: &Path, user: &Path, explicit: bool, workspace: &Path) -> Result<Loaded, String> {
    load_with_policy(home, user, explicit, workspace, policy_at(managed_path())?)
}

fn load_with_policy(
    home: &Path,
    user: &Path,
    explicit: bool,
    workspace: &Path,
    policy: Option<ManagedPolicy>,
) -> Result<Loaded, String> {
    assemble(home, user, explicit, workspace, policy, None)
}

pub fn for_workspace(
    home: &Path,
    user: &Path,
    base: &File,
    workspace: &Path,
) -> Result<Loaded, String> {
    assemble(
        home,
        user,
        false,
        workspace,
        base.policy.clone(),
        base.user_values.clone(),
    )
}

fn assemble(
    home: &Path,
    user: &Path,
    explicit: bool,
    workspace: &Path,
    policy: Option<ManagedPolicy>,
    snapshot: Option<toml::Table>,
) -> Result<Loaded, String> {
    let dirs = directories(workspace)?;
    let boundary = dirs[0].clone();
    let mut merged = Merged::default();
    if let Some(values) = snapshot {
        let text =
            toml::to_string(&values).map_err(|_| "cannot encode user configuration snapshot")?;
        merged
            .add(&text, &user.display().to_string(), Layer::User, true)
            .map_err(|e| e.to_string())?;
    } else {
        match std::fs::read_to_string(user) {
            Ok(text) => merged
                .add(&text, &user.display().to_string(), Layer::User, true)
                .map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {}
            Err(_) => return Err("user configuration is unavailable".into()),
        }
    }
    let user_values = merged.values.clone();
    let identity = if policy.as_ref().and_then(|p| p.allow_project_executables) == Some(false) {
        None
    } else {
        authorized_digest(home, user, workspace)?
    };
    let trusted = identity.is_some();
    let mut sources = Vec::new();
    for (name, layer) in [
        (".otto/config.toml", Layer::Shared),
        (".otto/config.local.toml", Layer::Local),
    ] {
        for dir in &dirs {
            if let Some(text) = project_read(dir, name)? {
                let path = dir.join(name);
                merged
                    .add(&text, &path.display().to_string(), layer, trusted)
                    .map_err(|e| e.to_string())?;
                sources.push(path);
            }
        }
    }
    if let Some(p) = &policy {
        p.apply(&mut merged, &managed_path().display().to_string())
            .map_err(|e| e.to_string())?;
    }
    let mut file = merged.file().map_err(|e| e.to_string())?;
    file.project_executables = Some(trusted);
    file.executable_digest = identity;
    file.policy = policy;
    file.user_values = Some(user_values);
    file.user_source = Some(user.display().to_string());
    file.diagnostics = merged.diagnostics.clone();
    Ok(Loaded {
        file,
        merged,
        boundary,
        sources,
        trusted,
    })
}

pub fn catalog_roots(
    file: &File,
    home: &Path,
    workspace: &Path,
    configured: &[String],
    kind: &str,
) -> Result<Vec<String>, String> {
    if file.project_executables.is_none() {
        return Ok(configured.to_vec());
    }
    let dirs = directories(workspace)?;
    let boundary = &dirs[0];
    let user_root = home.join(".otto").join(kind);
    let defaults = match kind {
        "skills" => file.skills.paths.is_none() && file.skills.enabled != Some(false),
        "agents" => file.agents.paths.is_none() && file.agents.enabled != Some(false),
        _ => false,
    };
    let lexical_workspace = std::path::absolute(workspace).map_err(|_| "invalid workspace path")?;
    let lexical_dirs: Vec<_> = lexical_workspace.ancestors().take(dirs.len()).collect();
    let physical = |p: &str| {
        if let Ok(path) = std::fs::canonicalize(p) {
            return path;
        }
        for (alias, canonical) in lexical_dirs.iter().zip(dirs.iter().rev()) {
            if let Ok(rest) = Path::new(p).strip_prefix(alias) {
                return canonical.join(rest);
            }
        }
        PathBuf::from(p)
    };
    let mut roots: Vec<_> = configured
        .iter()
        .filter(|p| {
            if defaults {
                return Path::new(p) == user_root;
            }
            file.project_executables == Some(true)
                || !physical(p).starts_with(boundary)
                || Path::new(p).starts_with(&user_root)
        })
        .cloned()
        .collect();
    if defaults && file.project_executables == Some(true) {
        for dir in dirs {
            roots.push(dir.join(".otto").join(kind).display().to_string());
        }
    }
    let mut seen = BTreeSet::new();
    roots.retain(|p| seen.insert(p.clone()));
    Ok(roots)
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grants {
    projects: BTreeMap<String, BTreeSet<String>>,
}

fn trust_path(home: &Path) -> PathBuf {
    home.join(".config/otto/trust/executables.json")
}

fn grants(home: &Path) -> Result<(Vec<u8>, Grants), String> {
    let path = trust_path(home);
    let mut file = match crate::fsops::open_no_follow(&path, libc::O_RDONLY) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Grants::default()));
        }
        Err(_) => return Err("cannot read executable trust records".into()),
    };
    for ancestor in path.ancestors().take_while(|p| *p != home) {
        let info = std::fs::symlink_metadata(ancestor)
            .map_err(|_| "cannot inspect executable trust ownership")?;
        if info.file_type().is_symlink()
            || info.uid() != nix::unistd::Uid::effective().as_raw()
            || info.mode() & 0o022 != 0
        {
            return Err(
                "executable trust records must be user-owned and not writable by others".into(),
            );
        }
    }
    let bytes = bounded_read(&mut file)?.into_bytes();
    let value = serde_json::from_slice(&bytes).map_err(|_| "invalid executable trust records")?;
    Ok((bytes, value))
}

pub(crate) fn command_sources(
    hooks: &otto_core::config::plugins::Hooks,
    workspace: &Path,
) -> Vec<PathBuf> {
    hooks
        .entries()
        .into_iter()
        .flat_map(|(_, entries)| entries.iter())
        .flat_map(|h| h.command.iter())
        .map(|arg| workspace.join(arg))
        .filter(|path| {
            path.starts_with(workspace)
                && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() || m.is_symlink())
        })
        .collect()
}

fn digest(
    home: &Path,
    user: &Path,
    workspace: &Path,
) -> Result<(String, String, Vec<String>), String> {
    let dirs = directories(workspace)?;
    let root = dirs[0].display().to_string();
    let mut hash = Sha256::new();
    let mut sources = Vec::new();
    let mut hook_sources = Vec::new();
    // Bind absence as well as definitions; config prefs outside MCP identity do not invalidate.
    for dir in &dirs {
        for relative in [".otto/config.toml", ".otto/config.local.toml"] {
            if let Some(text) = project_read(dir, relative)? {
                let raw: toml::Table =
                    toml::from_str(&text).map_err(|_| "invalid project configuration")?;
                if let Some(hooks) = raw.get("hooks") {
                    let hooks: otto_core::config::plugins::Hooks = hooks
                        .clone()
                        .try_into()
                        .map_err(|_| "invalid hook configuration")?;
                    hook_sources.extend(command_sources(&hooks, dirs.last().expect("workspace")));
                    hash.update(dir.join(relative).as_os_str().as_encoded_bytes());
                    hash.update(serde_json::to_vec(&hooks).map_err(|_| "cannot digest hooks")?);
                    sources.push(dir.join(relative).display().to_string());
                }
                if let Some(servers) = raw
                    .get("mcp")
                    .and_then(toml::Value::as_table)
                    .and_then(|m| m.get("servers"))
                    .and_then(toml::Value::as_table)
                {
                    let mut identities = toml::Table::new();
                    for (name, server) in servers {
                        if let Some(fields) = server.as_table() {
                            let entry: toml::Table = fields
                                .iter()
                                .filter(|(key, _)| {
                                    ["enabled", "transport", "command", "args", "cwd", "url"]
                                        .contains(&key.as_str())
                                })
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            if !entry.is_empty() {
                                identities.insert(name.clone(), toml::Value::Table(entry));
                            }
                        }
                    }
                    if !identities.is_empty() {
                        let path = dir.join(relative).display().to_string();
                        hash.update(path.as_bytes());
                        hash.update([0]);
                        hash.update(
                            serde_json::to_vec(&identities)
                                .map_err(|_| "cannot digest project configuration")?,
                        );
                        sources.push(path);
                    }
                }
            }
        }
    }
    // Custom roots come only from user/managed configuration, but contents inside the
    // repository are still project executables and are bound to this grant.
    let mut merged = Merged::default();
    if let Ok(text) = std::fs::read_to_string(user) {
        merged
            .add(&text, &user.display().to_string(), Layer::User, true)
            .map_err(|e| e.to_string())?;
    } else if user.exists() {
        return Err("cannot read user configuration for executable trust".into());
    }
    if let Some(policy) = policy_at(managed_path())? {
        policy
            .apply(&mut merged, &managed_path().display().to_string())
            .map_err(|e| e.to_string())?;
    }
    let file = merged.file().map_err(|e| e.to_string())?;
    let env = std::collections::HashMap::from([("HOME".into(), home.display().to_string())]);
    let canonical_workspace = dirs
        .last()
        .expect("nonempty directory chain")
        .display()
        .to_string();
    let skills = otto_core::config::resolve_skills(&file, &env, &canonical_workspace);
    let agents = otto_core::config::resolve_agents(&file, &env, &canonical_workspace)
        .map_err(|e| e.to_string())?;
    let mut roots: BTreeSet<PathBuf> = skills
        .roots
        .into_iter()
        .chain(agents.roots)
        .map(PathBuf::from)
        .collect();
    for dir in &dirs {
        roots.insert(dir.join(".otto/hooks"));
        if file.skills.paths.is_none() && skills.enabled {
            roots.insert(dir.join(".otto/skills"));
        }
        if file.agents.paths.is_none() && agents.enabled {
            roots.insert(dir.join(".otto/agents"));
        }
    }
    roots.extend(hook_sources);
    roots.extend(command_sources(
        &file.hooks,
        dirs.last().expect("workspace"),
    ));
    let mut total_bytes = 0usize;
    let pinned = Workspace::new(&dirs[0]).map_err(|_| "cannot pin executable boundary")?;
    for path in roots {
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !canonical.starts_with(&dirs[0]) || path.starts_with(home.join(".otto")) {
            if path.starts_with(&dirs[0]) && !canonical.starts_with(&dirs[0]) {
                return Err("project executable directory escapes repository".into());
            }
            continue;
        }
        hash.update(path.to_string_lossy().as_bytes());
        hash.update([0]);
        digest_tree(&path, &pinned, &mut hash, &mut sources, &mut total_bytes, 0)?;
    }
    Ok((root, format!("{:x}", hash.finalize()), sources))
}

fn digest_tree(
    path: &Path,
    pinned: &Workspace,
    hash: &mut Sha256,
    sources: &mut Vec<String>,
    total_bytes: &mut usize,
    depth: usize,
) -> Result<(), String> {
    let m = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("cannot inspect project executable source".into()),
    };
    if m.file_type().is_symlink() || depth > 32 || sources.len() >= 4096 {
        return Err("unsafe or oversized project executable sources".into());
    }
    if m.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .map_err(|_| "cannot inspect executable directory")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "cannot inspect executable entry")?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            digest_tree(&entry.path(), pinned, hash, sources, total_bytes, depth + 1)?;
        }
    } else if m.is_file() {
        let file = pinned
            .open(path)
            .map_err(|_| "cannot pin executable source")?;
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot digest executable source")?;
        if bytes.len() as u64 > MAX_CONFIG {
            return Err("executable source exceeds 1 MiB".into());
        }
        *total_bytes += bytes.len();
        if *total_bytes > 16 * 1024 * 1024 {
            return Err("project executable sources exceed 16 MiB total".into());
        }
        let source = path.display().to_string();
        hash.update(path.as_os_str().as_bytes());
        hash.update([0]);
        hash.update(&bytes);
        hash.update([0]);
        sources.push(source);
    } else {
        return Err("project executable source is not a regular file".into());
    }
    Ok(())
}

pub fn approved(home: &Path, workspace: &Path) -> Result<bool, String> {
    approved_for(home, &home.join(".config/otto/config.toml"), workspace)
}

pub fn approved_for(home: &Path, user: &Path, workspace: &Path) -> Result<bool, String> {
    authorized_digest(home, user, workspace).map(|identity| identity.is_some())
}

pub fn authorized_digest(
    home: &Path,
    user: &Path,
    workspace: &Path,
) -> Result<Option<String>, String> {
    let (_, records) = grants(home)?;
    if records.projects.is_empty() {
        return Ok(None);
    }
    let boundary = directories(workspace)?[0].display().to_string();
    if !records.projects.contains_key(&boundary) {
        return Ok(None);
    }
    let (root, identity, _) = digest(home, user, workspace)?;
    Ok(records
        .projects
        .get(&root)
        .is_some_and(|v| v.contains(&identity))
        .then_some(identity))
}

pub fn set_trust(home: &Path, workspace: &Path, enable: bool) -> Result<Vec<String>, String> {
    set_trust_for(
        home,
        &home.join(".config/otto/config.toml"),
        workspace,
        enable,
    )
}

pub fn set_trust_for(
    home: &Path,
    user: &Path,
    workspace: &Path,
    enable: bool,
) -> Result<Vec<String>, String> {
    let (root, identity, sources) = if enable {
        digest(home, user, workspace)?
    } else {
        (
            directories(workspace)?[0].display().to_string(),
            String::new(),
            Vec::new(),
        )
    };
    if enable
        && policy_at(managed_path())?.is_some_and(|p| p.allow_project_executables == Some(false))
    {
        return Err("managed policy prohibits project executables".into());
    }
    let (original, mut records) = grants(home)?;
    if enable {
        records.projects.entry(root).or_default().insert(identity);
    } else {
        records.projects.remove(&root);
    }
    let bytes = serde_json::to_vec_pretty(&records).map_err(|_| "cannot encode trust records")?;
    super::write_bytes(&trust_path(home), &original, &bytes)
        .map_err(|_| "cannot save trust records")?;
    Ok(sources)
}

pub fn instruction_files(home: &Path, workspace: &Path) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let user = home.join(".config/otto");
    if let Some(text) = project_read(&user, "AGENTS.md")? {
        out.push((user.join("AGENTS.md").display().to_string(), text));
    }
    for dir in directories(workspace)? {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            if let Some(text) = project_read(&dir, name)? {
                out.push((dir.join(name).display().to_string(), text));
                break;
            }
        }
    }
    if out.iter().any(|(_, t)| t.len() > 16384)
        || out.iter().map(|(_, t)| t.len()).sum::<usize>() > 65536
    {
        return Err("instruction file budget exceeded (16 KiB per file, 64 KiB total)".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
