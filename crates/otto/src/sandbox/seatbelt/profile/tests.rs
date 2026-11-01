use super::*;
use crate::sandbox::seatbelt::state;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// [`Dependencies`] borrows its callbacks, so the fixture owns them and
/// hands out a borrowing view.
/// The fixture's owned resolver, matching [`Dependencies::resolve`].
type ResolveFn = dyn Fn(&str) -> Result<ResolvedPath, ResolveError>;

struct Deps {
    resolve: Box<ResolveFn>,
}

impl Deps {
    fn new() -> Self {
        Self {
            resolve: Box::new(resolve_path),
        }
    }

    fn view(&self) -> Dependencies<'_> {
        Dependencies {
            resolve: self.resolve.as_ref(),
        }
    }

    /// Layers one lookup over the current resolver. `None` falls through to
    /// the resolver that was installed before.
    fn override_resolve<F>(&mut self, lookup: F)
    where
        F: Fn(&str) -> Option<Result<ResolvedPath, ResolveError>> + 'static,
    {
        let base = std::mem::replace(&mut self.resolve, Box::new(resolve_path));
        self.resolve = Box::new(move |path| lookup(path).unwrap_or_else(|| base(path)));
    }

    fn with_synthetic_directories(&mut self, paths: &[String]) {
        let mut resolved: HashMap<String, ResolvedPath> = HashMap::new();
        for path in paths {
            resolved.insert(
                path.clone(),
                ResolvedPath {
                    path: path.clone(),
                    kind: PathKind::Directory,
                    executable: false,
                },
            );
        }
        self.override_resolve(move |path| resolved.get(path).cloned().map(Ok));
    }
}

/// Dropping it closes the private state before the temporary tree is
/// removed, which is what `t.Cleanup` does there.
struct Fixture {
    temp: Option<tempfile::TempDir>,
    base: String,
    state: Option<state::State>,
    options: Options,
    deps: Deps,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let result = self.state.take().expect("state").close();
        drop(self.temp.take());
        if !std::thread::panicking() {
            assert!(result.is_ok(), "state close failed: {result:?}");
        }
    }
}

impl Fixture {
    fn state(&self) -> &state::State {
        self.state.as_ref().expect("state")
    }

    fn render(&self) -> String {
        self.generated().text
    }

    fn generated(&self) -> GeneratedProfile {
        generate_with(&self.options, &self.deps.view()).expect("generate profile")
    }

    fn generate(&self) -> Result<GeneratedProfile, Rejected> {
        generate_with(&self.options, &self.deps.view())
    }

    fn assert_rejected(&self, what: &str) {
        match self.generate() {
            Err(Rejected) => {}
            Ok(profile) => panic!("{what} generated {} bytes", profile.text.len()),
        }
    }

    fn directory(&self, path: &std::path::Path) -> String {
        path.to_str().expect("utf-8 directory").to_string()
    }
}

fn new_fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let base = canonical(temp.path().to_str().expect("utf-8 temp dir"));
    let workspace = make_directory(&join(&base, "workspace"));
    let cache_base = make_directory(&join(&base, "user-cache"));
    let private_state = state::create(&workspace, &cache_base).expect("create private state");
    let home = make_directory(&join(&base, "resolved-home"));
    let shell = make_file(&join(&join(&base, "shells"), "shell"), 0o700);
    let options = Options {
        workspace: workspace.clone(),
        workspaces: vec![workspace],
        directories: private_state.directories.clone(),
        shell,
        home,
        read_paths: Vec::new(),
        deny_read_paths: Vec::new(),
        network: Some(NetworkMode::Deny),
    };
    Fixture {
        temp: Some(temp),
        base,
        state: Some(private_state),
        options,
        deps: Deps::new(),
    }
}

fn canonical(path: &str) -> String {
    std::fs::canonicalize(path)
        .expect("canonicalize")
        .to_str()
        .expect("utf-8 path")
        .to_string()
}

fn make_directory(path: &str) -> String {
    std::fs::create_dir_all(path).expect("create directory");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("chmod directory");
    canonical(path)
}

fn make_file(path: &str, mode: u32) -> String {
    std::fs::create_dir_all(dir(path)).expect("create parent");
    std::fs::write(path, b"fixture").expect("write file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod file");
    canonical(path)
}

fn symlink(target: &str, link: &str) {
    std::os::unix::fs::symlink(target, link).expect("symlink");
}

fn section<'a>(profile: &'a str, name: &str) -> &'a str {
    let begin = format!("; OTTO-DYNAMIC-{name}-BEGIN");
    let end = format!("; OTTO-DYNAMIC-{name}-END");
    let start = profile
        .find(&begin)
        .unwrap_or_else(|| panic!("profile section {name} not found"));
    let finish = profile
        .find(&end)
        .unwrap_or_else(|| panic!("profile section {name} not found"));
    assert!(finish >= start, "profile section {name} is inverted");
    &profile[start..finish + end.len()]
}

fn quote(path: &str) -> String {
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

fn filter(kind: &str, path: &str) -> String {
    format!("({kind} {})", quote(path))
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

mod deny_rules;
mod dynamic_literals;
mod read_policy;
mod system_rules;
