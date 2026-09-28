//! The `[failover]` table and its resolution.
//!
//! Top-level only: a session's lease directory, once created, fixes its own
//! `lease_seconds`, so per-profile or per-project failover settings would
//! only ever apply to a session's first creation and would be misleading
//! everywhere else. See `docs/specs/2026-09-28-session-failover.md`.

use serde::{Deserialize, Serialize};

use super::ConfigError;

/// Minimum accepted `lease_seconds`: at `L`/3 the renewal thread must clear
/// the 1 s watchdog tick plus the acquirer's 1 s heartbeat poll with margin,
/// so `L`/3 must exceed 2 s, i.e. `L` >= 6; this doubles that floor for
/// operating margin.
pub const MINIMUM_LEASE_SECONDS: u64 = 12;

/// The `[failover]` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failover {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
}

impl Default for Failover {
    fn default() -> Self {
        Failover {
            enabled: false,
            lease_seconds: default_lease_seconds(),
        }
    }
}

/// Must agree with `otto::failover::lease::DEFAULT_LEASE_SECONDS`: otto-core
/// stays wasm-safe and cannot depend on the native `otto` crate that owns
/// the lease protocol, so the two constants are independent and pinned by
/// tests on both sides rather than shared.
fn default_lease_seconds() -> u64 {
    30
}

/// The resolved `[failover]` configuration for one process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FailoverRuntime {
    pub enabled: bool,
    pub lease_seconds: u64,
}

/// Resolves `[failover]`. Rejects a `lease_seconds` below
/// [`MINIMUM_LEASE_SECONDS`], naming the key and the minimum.
pub fn resolve_failover(file: &super::File) -> Result<FailoverRuntime, ConfigError> {
    if file.failover.lease_seconds < MINIMUM_LEASE_SECONDS {
        return Err(ConfigError::new(format!(
            "invalid failover.lease_seconds: must be at least {MINIMUM_LEASE_SECONDS}"
        )));
    }
    Ok(FailoverRuntime {
        enabled: file.failover.enabled,
        lease_seconds: file.failover.lease_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::File;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn defaults_to_disabled_with_thirty_second_lease() {
        let runtime = resolve_failover(&File::default()).expect("resolve");
        assert_eq!(
            runtime,
            FailoverRuntime {
                enabled: false,
                lease_seconds: 30,
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn explicit_table_is_used() {
        let file =
            super::super::parse("[failover]\nenabled = true\nlease_seconds = 60\n").expect("parse");
        let runtime = resolve_failover(&file).expect("resolve");
        assert_eq!(
            runtime,
            FailoverRuntime {
                enabled: true,
                lease_seconds: 60,
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_lease_seconds_below_the_minimum() {
        let file = super::super::parse("[failover]\nlease_seconds = 11\n").expect("parse");
        let err = resolve_failover(&file).unwrap_err();
        assert!(err.to_string().contains("failover.lease_seconds"), "{err}");
        assert!(err.to_string().contains("12"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_key() {
        let err = super::super::parse("[failover]\nenabled = true\nbogus = 1\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }
}
