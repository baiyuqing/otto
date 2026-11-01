//! A temporary plugin home with source packages, for tests of the plugin
//! service and of the `otto plugin` and `otto mcp` commands.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::plugin::{Installed, Service, package};

/// A temporary `HOME` whose plugin state is thawed on drop.
pub struct Fixture {
    pub home: tempfile::TempDir,
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
        }
    }

    /// The plugin service over this home.
    pub fn service(&self) -> Service {
        Service::new(self.home.path())
    }

    /// A source package `name` at version 1.0.0 with one skill, `review`.
    pub fn package(&self, name: &str) -> PathBuf {
        let path = self.home.path().join(format!("source-{name}"));
        fs::create_dir_all(path.join("skills/review")).unwrap();
        fs::write(
            path.join("plugin.json"),
            json!({"$schema":package::PLUGIN_SCHEMA,"name":name,"version":"1.0.0"}).to_string(),
        )
        .unwrap();
        fs::write(
            path.join("skills/review/SKILL.md"),
            "---\nname: review\ndescription: Review changes\n---\nReview carefully.\n",
        )
        .unwrap();
        path
    }

    /// A source package `name` without skills whose `dev.otto` extension is
    /// `components`.
    pub fn package_with_extension(&self, name: &str, components: serde_json::Value) -> PathBuf {
        let path = self.home.path().join(format!("source-{name}"));
        fs::create_dir_all(&path).unwrap();
        let manifest = json!({
            "$schema": package::PLUGIN_SCHEMA,
            "name": name,
            "extensions": {"dev.otto": components}
        });
        fs::write(path.join("plugin.json"), manifest.to_string()).unwrap();
        path
    }

    /// Writes `mcp.json` with `servers` into the package at `path`.
    pub fn mcp(&self, path: &Path, servers: serde_json::Value) {
        fs::write(
            path.join("mcp.json"),
            json!({"$schema":package::MCP_SCHEMA,"mcpServers":servers}).to_string(),
        )
        .unwrap();
    }

    /// The environment of a command run against this home: `HOME` and a
    /// `PATH` of the system directories.
    pub fn env(&self) -> HashMap<String, String> {
        HashMap::from([
            (
                "HOME".into(),
                self.home.path().to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ])
    }

    /// Installs the source package at `path`, untrusted.
    pub fn install(&self, path: &Path) -> Installed {
        self.service()
            .install(path.to_str().unwrap(), None)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = crate::plugin::thaw(self.home.path());
    }
}
