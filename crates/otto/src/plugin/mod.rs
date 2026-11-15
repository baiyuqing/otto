//! Native plugin snapshots, registry and content-bound execution authorization.
//! Installation never executes package code. Runners retain immutable snapshots;
//! removal leaves snapshots/data intact so existing sessions keep pinned files.
pub mod claude_hooks;
pub mod hooks;
pub mod package;
pub(crate) mod runtime;
use crate::hex::to_hex;
use crate::{
    fsops,
    fsops::root::{self, Root},
};
use nix::{fcntl::OFlag, sys::stat::Mode};
use package::Package;
pub use runtime::internal_name as runtime_name;
pub use runtime::{GuardedServer, Snapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_FILES: usize = 10000;
const MAX_BYTES: u64 = 256 << 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub name: String,
    pub version: Option<String>,
    pub source: String,
    pub commit: Option<String>,
    pub digest: String,
    pub enabled: bool,
    pub trusted: bool,
    pub package: Package,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    #[serde(default)]
    packages: BTreeMap<String, Installed>,
}
#[derive(Clone)]
pub struct Service {
    root: PathBuf,
}
impl Service {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".otto/plugins"),
        }
    }
    pub fn package_path(&self, digest: &str) -> PathBuf {
        self.root.join("packages").join(digest)
    }
    pub fn data_path(&self, name: &str) -> PathBuf {
        self.root.join("data").join(name)
    }
    fn open(&self) -> Result<fsops::Dir, String> {
        let home = self.root.parent().ok_or("invalid plugin state root")?;
        fs::create_dir_all(home).map_err(|_| "cannot create Otto state directory")?;
        let root = Root::open(home).map_err(|_| "cannot open Otto state directory")?;
        root.mkdir_all(
            Path::new("plugins/packages"),
            Mode::from_bits_truncate(0o700),
        )
        .map_err(|_| "cannot create plugin packages directory")?;
        root.mkdir_all(Path::new("plugins/data"), Mode::from_bits_truncate(0o700))
            .map_err(|_| "cannot create plugin data directory")?;
        fsops::open_dir_no_follow(&self.root)
            .map_err(|_| "cannot open plugin state directory".into())
    }
    fn registry(&self) -> Result<Registry, String> {
        let root = match Root::open(&self.root) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Registry::default()),
            Err(_) => return Err("cannot open plugin registry".into()),
        };
        let bytes = match package::read(&root, Path::new("registry.json")) {
            Ok(b) => b,
            Err(_) if root.lstat(Path::new("registry.json")).is_err() => {
                return Ok(Registry::default());
            }
            Err(e) => return Err(e),
        };
        let registry: Registry =
            serde_json::from_slice(&bytes).map_err(|_| "invalid plugin registry")?;
        for (name, p) in &registry.packages {
            if name != &p.name
                || !package::valid_name(name)
                || !valid_digest(&p.digest)
                || p.package.name != *name
            {
                return Err("invalid plugin registry identity".into());
            }
        }
        Ok(registry)
    }
    fn save(&self, dir: &fsops::Dir, registry: &Registry) -> Result<(), String> {
        let bytes =
            serde_json::to_vec_pretty(registry).map_err(|_| "cannot encode plugin registry")?;
        if bytes.len() > 1 << 20 {
            return Err("plugin registry exceeds 1 MiB".into());
        }
        let staged_registry = tempfile::Builder::new()
            .prefix(".registry-")
            .tempfile_in(&self.root)
            .map_err(|_| "cannot stage plugin registry")?;
        let name = staged_registry
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid staging name")?
            .to_owned();
        let mut file = staged_registry.as_file();
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "cannot write plugin registry")?;
        fsops::rename_at(dir, &name, "registry.json")
            .map_err(|_| "cannot publish plugin registry")?;
        Ok(())
    }
    pub fn list(&self) -> Result<Vec<Installed>, String> {
        Ok(self.registry()?.packages.into_values().collect())
    }
    pub fn inspect(&self, name: &str) -> Result<Installed, String> {
        self.registry()?
            .packages
            .remove(name)
            .ok_or_else(|| "plugin is not installed".into())
    }
    pub fn install(&self, source: &str, revision: Option<&str>) -> Result<Installed, String> {
        let dir = self.open()?;
        let _lock = fsops::lock_directory(&dir).map_err(|_| "plugin registry is busy")?;
        let mut registry = self.registry()?;
        let checkout = tempfile::tempdir().map_err(|_| "cannot stage Git checkout")?;
        let (input, canonical_source, commit) = if let Some(rev) = revision {
            if !valid_digest_len(rev, 40) && !valid_digest_len(rev, 64) {
                return Err("Git revision must be a full hexadecimal commit ID".into());
            }
            validate_git_source(source)?;
            let target = checkout.path().join("repo");
            run_git(
                &[
                    "clone",
                    "--no-checkout",
                    "--no-recurse-submodules",
                    "--",
                    source,
                    target.to_str().ok_or("invalid Git staging path")?,
                ],
                None,
            )?;
            run_git(&["checkout", "--detach", rev], Some(&target))?;
            let actual = run_git(&["rev-parse", "HEAD"], Some(&target))?;
            if actual.trim() != rev.to_ascii_lowercase() {
                return Err("Git checkout did not match requested commit".into());
            }
            (target, source.to_owned(), Some(actual.trim().to_owned()))
        } else {
            let input =
                fs::canonicalize(source).map_err(|_| "local plugin directory is unavailable")?;
            (input.clone(), input.to_string_lossy().into_owned(), None)
        };
        let package = package::discover(&input)?;
        if registry.packages.contains_key(&package.name) {
            return Err("plugin name is already installed; remove it before replacing".into());
        }
        let packages_dir = fsops::open_dir_no_follow(&self.root.join("packages"))
            .map_err(|_| "cannot open plugin snapshots")?;
        let staging = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(self.root.join("packages"))
            .map_err(|_| "cannot stage plugin snapshot")?;
        copy_package(&input, staging.path())?;
        let published_package = package::discover(staging.path())?;
        if published_package.name != package.name {
            return Err("plugin changed during installation".into());
        }
        let digest = digest(staging.path())?;
        freeze(staging.path())?;
        let staged_name = staging
            .path()
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid snapshot staging name")?;
        if !self.package_path(&digest).exists() {
            fsops::publish_directory(&packages_dir, staged_name, &digest, false)
                .map_err(|_| "cannot publish plugin snapshot")?;
        } else if digest != self::digest(&self.package_path(&digest))? {
            return Err("existing plugin snapshot was modified".into());
        }
        // TempDir cannot remove a read-only duplicate tree; make only that unpublished
        // staging copy writable, never an already published snapshot.
        if staging.path().exists() {
            thaw(staging.path())?;
        }
        let installed = Installed {
            name: published_package.name.clone(),
            version: published_package.version.clone(),
            source: canonical_source,
            commit,
            digest,
            enabled: true,
            trusted: false,
            package: published_package,
        };
        registry
            .packages
            .insert(installed.name.clone(), installed.clone());
        self.save(&dir, &registry)?;
        Ok(installed)
    }
    pub fn change(&self, name: &str, action: &str) -> Result<(), String> {
        let dir = self.open()?;
        let _lock = fsops::lock_directory(&dir).map_err(|_| "plugin registry is busy")?;
        let mut registry = self.registry()?;
        let installed = registry
            .packages
            .get_mut(name)
            .ok_or("plugin is not installed")?;
        match action {
            "enable" => installed.enabled = true,
            "disable" => installed.enabled = false,
            "trust" => {
                if digest(&self.package_path(&installed.digest))? != installed.digest {
                    return Err("plugin content changed; reinstall before trusting".into());
                }
                installed.trusted = true;
            }
            "revoke" => installed.trusted = false,
            "remove" => {
                registry.packages.remove(name);
            }
            _ => return Err("unknown plugin action".into()),
        }
        self.save(&dir, &registry)
    }
    pub fn authorized(&self, name: &str, expected: &str) -> bool {
        self.inspect(name)
            .is_ok_and(|p| p.trusted && p.digest == expected)
            && digest(&self.package_path(expected)).is_ok_and(|d| d == expected)
    }
    pub(crate) fn ensure_data(&self, name: &str) -> Result<PathBuf, String> {
        if !package::valid_name(name) {
            return Err("invalid plugin name".into());
        }
        let _dir = self.open()?;
        let root =
            Root::open(&self.root.join("data")).map_err(|_| "cannot open plugin data root")?;
        root.mkdir_all(Path::new(name), Mode::from_bits_truncate(0o700))
            .map_err(|_| "cannot create plugin data directory")?;
        fs::canonicalize(self.data_path(name))
            .map_err(|_| "cannot resolve plugin data directory".into())
    }
}
fn valid_digest_len(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_digest(s: &str) -> bool {
    valid_digest_len(s, 64) && !s.bytes().any(|b| b.is_ascii_uppercase())
}
fn validate_git_source(source: &str) -> Result<(), String> {
    if source.starts_with('-')
        || source.contains('\n')
        || source.contains('\r')
        || source.contains("::")
    {
        return Err("unsupported Git source".into());
    }
    if source.contains("://") {
        let url = url::Url::parse(source).map_err(|_| "invalid Git URL")?;
        if !matches!(url.scheme(), "https" | "ssh" | "file")
            || url.password().is_some()
            || (url.scheme() == "https" && !url.username().is_empty())
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Git URL must not contain credentials or query parameters".into());
        }
    } else if source.contains('@') && !source.starts_with("git@") {
        return Err("unsupported Git authentication in URL".into());
    }
    Ok(())
}
fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.ext.allow=never",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|_| "cannot start Git")?;
    let start = Instant::now();
    loop {
        if child
            .try_wait()
            .map_err(|_| "cannot wait for Git")?
            .is_some()
        {
            break;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Git operation timed out".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().map_err(|_| "cannot finish Git")?;
    if !output.status.success() {
        return Err("Git operation failed (check source, commit and authentication)".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "invalid Git response".into())
}
fn copy_package(input: &Path, output: &Path) -> Result<(), String> {
    let root = Root::open(input).map_err(|_| "cannot open source package")?;
    let mut budget = (0usize, 0u64);
    fn walk(
        root: &Root,
        relative: &Path,
        output: &Path,
        depth: usize,
        budget: &mut (usize, u64),
    ) -> Result<(), String> {
        if depth > 32 {
            return Err("package directory nesting or symlink cycle exceeds limit".into());
        }
        let mut entries = root
            .read_dir(relative)
            .map_err(|_| "cannot enumerate package")?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            if entry.name == ".git" {
                continue;
            }
            budget.0 += 1;
            if budget.0 > MAX_FILES {
                return Err("package exceeds file limit".into());
            }
            let path = relative.join(&entry.name);
            let stat = root
                .stat(&path)
                .map_err(|_| "package path escapes root or is unreadable")?;
            let target = output.join(&entry.name);
            if root::is_dir(&stat) {
                fs::create_dir(&target).map_err(|_| "cannot create snapshot directory")?;
                walk(root, &path, &target, depth + 1, budget)?;
            } else if root::is_regular(&stat) {
                let mut file = root
                    .open_file(&path, OFlag::O_RDONLY | OFlag::O_NONBLOCK, Mode::empty())
                    .map_err(|_| "cannot read package file")?;
                if !file
                    .metadata()
                    .map_err(|_| "cannot inspect source file")?
                    .is_file()
                {
                    return Err("source file is no longer regular".into());
                }
                let mut bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take(MAX_BYTES - budget.1 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "cannot copy package file")?;
                budget.1 += bytes.len() as u64;
                if budget.1 > MAX_BYTES {
                    return Err("package exceeds 256 MiB".into());
                }
                let mode = if file
                    .metadata()
                    .map_err(|_| "cannot inspect package file")?
                    .permissions()
                    .mode()
                    & 0o111
                    != 0
                {
                    0o755
                } else {
                    0o644
                };
                fs::write(&target, bytes).map_err(|_| "cannot write snapshot file")?;
                fs::set_permissions(&target, fs::Permissions::from_mode(mode))
                    .map_err(|_| "cannot set snapshot permissions")?;
            } else {
                return Err("package contains a non-regular file".into());
            }
        }
        Ok(())
    }
    walk(&root, Path::new("."), output, 0, &mut budget)
}
/// Digest pinned reads, executable bits, and relative names. Symlink changes are
/// rejected: installation materializes safe links into ordinary files/directories.
pub(crate) fn digest(path: &Path) -> Result<String, String> {
    let root = Root::open(path).map_err(|_| "cannot open package snapshot")?;
    let mut hash = Sha256::new();
    let mut count = 0;
    let mut total = 0u64;
    fn walk(
        root: &Root,
        path: &Path,
        hash: &mut Sha256,
        count: &mut usize,
        total: &mut u64,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 32 {
            return Err("snapshot exceeds nesting limit".into());
        }
        let mut entries = root
            .read_dir(path)
            .map_err(|_| "cannot enumerate snapshot")?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            *count += 1;
            if *count > MAX_FILES || entry.is_symlink {
                return Err("snapshot modified or exceeds file limit".into());
            }
            let relative = path.join(entry.name);
            let bytes = relative.as_os_str().as_bytes();
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
            if entry.is_dir {
                hash.update(b"d");
                walk(root, &relative, hash, count, total, depth + 1)?;
            } else if entry.is_regular {
                let mut file = root
                    .open_file(
                        &relative,
                        OFlag::O_RDONLY | OFlag::O_NONBLOCK,
                        Mode::empty(),
                    )
                    .map_err(|_| "cannot read snapshot")?;
                let metadata = file.metadata().map_err(|_| "cannot inspect snapshot")?;
                if !metadata.is_file() {
                    return Err("snapshot file changed".into());
                }
                let mut snapshot_file_bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take(MAX_BYTES - *total + 1)
                    .read_to_end(&mut snapshot_file_bytes)
                    .map_err(|_| "cannot hash snapshot")?;
                *total += snapshot_file_bytes.len() as u64;
                if *total > MAX_BYTES {
                    return Err("snapshot exceeds byte limit".into());
                }
                hash.update(b"f");
                hash.update([u8::from(metadata.permissions().mode() & 0o111 != 0)]);
                hash.update((snapshot_file_bytes.len() as u64).to_le_bytes());
                hash.update(snapshot_file_bytes);
            } else {
                return Err("snapshot contains a non-regular file".into());
            }
        }
        Ok(())
    }
    walk(&root, Path::new("."), &mut hash, &mut count, &mut total, 0)?;
    Ok(to_hex(&hash.finalize()))
}
fn permissions_tree(path: &Path, writable: bool) -> Result<(), String> {
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if writable { 0o755 } else { 0o555 }),
    )
    .map_err(|_| "cannot protect snapshot")?;
    for entry in fs::read_dir(path).map_err(|_| "cannot inspect snapshot permissions")? {
        let path = entry.map_err(|_| "cannot inspect snapshot entry")?.path();
        let m = fs::symlink_metadata(&path).map_err(|_| "cannot inspect snapshot mode")?;
        if m.is_dir() {
            permissions_tree(&path, writable)?;
        } else if m.is_file() {
            fs::set_permissions(
                &path,
                fs::Permissions::from_mode(if m.permissions().mode() & 0o111 != 0 {
                    if writable { 0o755 } else { 0o555 }
                } else if writable {
                    0o644
                } else {
                    0o444
                }),
            )
            .map_err(|_| "cannot protect snapshot file")?;
        } else {
            return Err("snapshot contains an unexpected file".into());
        }
    }
    Ok(())
}
fn freeze(path: &Path) -> Result<(), String> {
    permissions_tree(path, false)
}
pub fn thaw(path: &Path) -> Result<(), String> {
    permissions_tree(path, true)
}

#[cfg(test)]
mod package_environment_tests;
#[cfg(test)]
mod tests;
