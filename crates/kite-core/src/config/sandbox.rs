//! Resolution of the `[sandbox]` table into validated, sorted settings.
//!
//! ponytail: these types mirror `kite::sandbox`'s own `DriverMode`,
//! `NetworkMode` and `Settings` field for field rather than reusing them,
//! because kite-core cannot depend on the native `kite` crate that owns the
//! sandbox executor. The native layer converts a [`SandboxSettings`] into its
//! own `sandbox::Settings` at the call site.

use super::{ConfigError, SandboxConfig};

const MAX_SANDBOX_READ_PATH_BYTES: usize = 32 * 1024;

/// Mirrors `kite::sandbox`'s three driver modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDriverMode {
    Auto,
    Seatbelt,
    Off,
}

/// Mirrors `kite::sandbox`'s two network modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxNetworkMode {
    Deny,
    Allow,
}

/// The resolved, validated `[sandbox]` settings, ready to convert into a
/// native `sandbox::Settings`. `read_paths` and `allow_env` are sorted and
/// own their storage independently of the input `SandboxConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSettings {
    pub driver: SandboxDriverMode,
    pub network: SandboxNetworkMode,
    pub read_paths: Vec<String>,
    pub allow_env: Vec<String>,
}

/// Resolves `raw` (the `[sandbox]` table) and an optional `--sandbox` CLI
/// flag value into [`SandboxSettings`]. `cli_driver` wins over
/// `raw.driver`, which wins over the `auto` default. An explicit empty
/// string for either mode is rejected; only an absent key falls back to a
/// default.
pub fn resolve_sandbox(
    raw: &SandboxConfig,
    cli_driver: Option<&str>,
) -> Result<SandboxSettings, ConfigError> {
    let mut driver_value = "auto";
    if let Some(value) = raw.driver.as_deref() {
        driver_value = value;
    }
    if let Some(value) = cli_driver {
        driver_value = value;
    }
    let driver = sandbox_driver_mode(driver_value)
        .ok_or_else(|| ConfigError::new("invalid sandbox driver"))?;

    let network_value = raw.network.as_deref().unwrap_or("allow");
    let network = sandbox_network_mode(network_value)
        .ok_or_else(|| ConfigError::new("invalid sandbox network"))?;

    let mut read_paths = raw.read_paths.clone();
    if !read_paths.iter().all(|path| valid_sandbox_read_path(path)) {
        return Err(ConfigError::new("invalid sandbox read_paths"));
    }
    read_paths.sort();

    let mut allow_env = raw.allow_env.clone();
    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(allow_env.len());
    for name in &allow_env {
        if !valid_sandbox_environment_name(name) || !seen.insert(name.as_str()) {
            return Err(ConfigError::new("invalid sandbox allow_env"));
        }
    }
    allow_env.sort();

    Ok(SandboxSettings {
        driver,
        network,
        read_paths,
        allow_env,
    })
}

fn sandbox_driver_mode(value: &str) -> Option<SandboxDriverMode> {
    match value {
        "auto" => Some(SandboxDriverMode::Auto),
        "seatbelt" => Some(SandboxDriverMode::Seatbelt),
        "off" => Some(SandboxDriverMode::Off),
        _ => None,
    }
}

fn sandbox_network_mode(value: &str) -> Option<SandboxNetworkMode> {
    match value {
        "allow" => Some(SandboxNetworkMode::Allow),
        "deny" => Some(SandboxNetworkMode::Deny),
        _ => None,
    }
}

fn valid_sandbox_read_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_SANDBOX_READ_PATH_BYTES || path.contains('\0') {
        return false;
    }
    super::paths::is_abs(path) || path.starts_with("~/")
}

fn valid_sandbox_environment_name(name: &str) -> bool {
    let mut chars = name.bytes();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(
        driver: Option<&str>,
        network: Option<&str>,
        read_paths: &[&str],
        allow_env: &[&str],
    ) -> SandboxConfig {
        SandboxConfig {
            driver: driver.map(String::from),
            network: network.map(String::from),
            read_paths: read_paths.iter().map(|s| s.to_string()).collect(),
            allow_env: allow_env.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn defaults_to_auto_allow_and_empty_lists() {
        let got = resolve_sandbox(&SandboxConfig::default(), None).expect("resolve");
        assert_eq!(
            got,
            SandboxSettings {
                driver: SandboxDriverMode::Auto,
                network: SandboxNetworkMode::Allow,
                read_paths: vec![],
                allow_env: vec![]
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_and_sorts_valid_settings() {
        let raw = config(
            Some("seatbelt"),
            Some("deny"),
            &["~/zeta", "/opt/zeta", "/opt/alpha"],
            &["ZETA_TOKEN", "ALPHA_TOKEN"],
        );
        let got = resolve_sandbox(&raw, None).expect("resolve");
        assert_eq!(got.driver, SandboxDriverMode::Seatbelt);
        assert_eq!(got.network, SandboxNetworkMode::Deny);
        assert_eq!(got.read_paths, vec!["/opt/alpha", "/opt/zeta", "~/zeta"]);
        assert_eq!(got.allow_env, vec!["ALPHA_TOKEN", "ZETA_TOKEN"]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_explicit_empty_modes() {
        assert!(resolve_sandbox(&config(Some(""), None, &[], &[]), None).is_err());
        assert!(resolve_sandbox(&config(None, Some(""), &[], &[]), None).is_err());
        assert!(resolve_sandbox(&SandboxConfig::default(), Some("")).is_err());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_driver_values() {
        for value in ["docker", "apple-container", "podman", "AUTO", "unknown"] {
            let err = resolve_sandbox(&config(Some(value), None, &[], &[]), None).unwrap_err();
            assert!(err.to_string().contains("driver"), "{value}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_network_values() {
        for value in ["block", "ALLOW", "off", "unknown"] {
            let err = resolve_sandbox(&config(None, Some(value), &[], &[]), None).unwrap_err();
            assert!(err.to_string().contains("network"), "{value}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_read_paths() {
        let too_long = format!("/{}", "x".repeat(32 * 1024));
        for path in [
            "relative/path",
            ".",
            "~",
            "~someone/source",
            "$HOME/source",
            "/safe\0unsafe",
            too_long.as_str(),
        ] {
            let err = resolve_sandbox(&config(None, None, &[path], &[]), None).unwrap_err();
            assert!(err.to_string().contains("read_paths"), "{path:?}: {err}");
        }

        let boundary = format!("/{}", "x".repeat(32 * 1024 - 1));
        let got = resolve_sandbox(&config(None, None, &[boundary.as_str()], &[]), None)
            .expect("boundary path accepted");
        assert_eq!(got.read_paths, vec![boundary]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_or_duplicate_allow_env() {
        let cases: &[&[&str]] = &[
            &[""],
            &["*_TOKEN"],
            &["PROJECT_*"],
            &["1PROJECT"],
            &["PROJECT-TOKEN"],
            &["PROJECT_TOKEN", "PROJECT_TOKEN"],
        ];
        for names in cases {
            let err = resolve_sandbox(&config(None, None, &[], names), None).unwrap_err();
            assert!(err.to_string().contains("allow_env"), "{names:?}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn clones_inputs_without_aliasing() {
        let raw = config(
            None,
            None,
            &["~/zeta", "/opt/alpha"],
            &["ZETA_TOKEN", "ALPHA_TOKEN"],
        );
        let got = resolve_sandbox(&raw, None).expect("resolve");
        // `raw` is untouched (Rust borrows immutably; this also checks the
        // resolved result copied rather than aliased the source Vecs).
        assert_eq!(raw.read_paths, vec!["~/zeta", "/opt/alpha"]);
        assert_eq!(raw.allow_env, vec!["ZETA_TOKEN", "ALPHA_TOKEN"]);
        assert_eq!(got.read_paths, vec!["/opt/alpha", "~/zeta"]);
        assert_eq!(got.allow_env, vec!["ALPHA_TOKEN", "ZETA_TOKEN"]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn cli_driver_takes_precedence_without_changing_other_settings() {
        let raw = config(
            Some("seatbelt"),
            Some("deny"),
            &["/opt/sdk"],
            &["PROJECT_TOKEN"],
        );
        let got = resolve_sandbox(&raw, Some("off")).expect("resolve");
        assert_eq!(got.driver, SandboxDriverMode::Off);
        assert_eq!(got.network, SandboxNetworkMode::Deny);
        assert_eq!(got.read_paths, vec!["/opt/sdk"]);
        assert_eq!(got.allow_env, vec!["PROJECT_TOKEN"]);
    }
}
