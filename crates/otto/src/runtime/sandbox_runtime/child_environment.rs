//! Host and child environment resolution for one sandbox open.
//!
//! The open path and `SandboxRuntime::reconfigure` both call these, so they
//! cannot compute different environments for the same options.

use otto_core::safetext::SecretCollector;

use crate::sandbox::PrivateDirectories;
use crate::sandbox::environment::{EnvironmentOptions, EnvironmentSnapshot, resolve_environment};

use super::OpenOptions;

/// The host environment classified before any driver opens.
pub(super) struct HostEnvironment {
    /// False when the host cannot be classified safely or yields no entries.
    pub(super) usable: bool,
    pub(super) redactions: Vec<String>,
    pub(super) complete: bool,
}

pub(super) fn resolve_host(options: &OpenOptions) -> HostEnvironment {
    let resolved = resolve_environment(&EnvironmentOptions {
        host_entries: options.host_entries.clone(),
        provider_names: options.provider_names.clone(),
        allow_names: options.settings.allow_env.clone(),
        private_directories: None,
    });
    let (snapshot, ok) = match &resolved {
        Ok(snapshot) => (snapshot, true),
        Err(rejected) => (rejected.snapshot(), false),
    };
    HostEnvironment {
        usable: ok && snapshot.entries().is_some(),
        redactions: snapshot.redaction_values().to_vec(),
        complete: snapshot.redactions_complete(),
    }
}

/// The child environment and the merged redaction set for one open.
pub(super) struct ChildEnvironment {
    /// False when resolution was rejected or produced no entries.
    pub(super) usable: bool,
    pub(super) entries: Option<Vec<String>>,
    pub(super) redactions: Vec<String>,
    pub(super) complete: bool,
}

/// Resolves the child environment, with `private_directories` when a driver
/// owns a private tree, and merges its redactions with the host's. The open
/// path and [`SandboxRuntime::reconfigure`] both call this, so they cannot
/// compute different environments for the same options.
pub(super) fn resolve_child(
    options: &OpenOptions,
    private_directories: Option<PrivateDirectories>,
    host_redactions: &[String],
    host_complete: bool,
) -> ChildEnvironment {
    let resolved = resolve_environment(&EnvironmentOptions {
        host_entries: options.host_entries.clone(),
        provider_names: options.provider_names.clone(),
        allow_names: options.settings.allow_env.clone(),
        private_directories,
    });
    let (snapshot, ok) = match &resolved {
        Ok(snapshot) => (snapshot, true),
        Err(rejected) => (rejected.snapshot(), false),
    };
    let (redactions, complete) = merged_redactions(host_redactions, snapshot, host_complete);
    let entries = snapshot.entries().map(<[String]>::to_vec);
    ChildEnvironment {
        usable: ok && entries.is_some(),
        entries,
        redactions,
        complete,
    }
}

/// Merges the host redaction set with the child's, keeping the union bounded.
/// The result is complete only if both inputs were complete and the merge
/// itself did not hit the collector's ceiling.
fn merged_redactions(
    host: &[String],
    snapshot: &EnvironmentSnapshot,
    host_complete: bool,
) -> (Vec<String>, bool) {
    let mut collector = SecretCollector::new();
    let mut merge_complete = true;
    'outer: for group in [host, snapshot.redaction_values()] {
        for value in group {
            if !collector.add_form(value) {
                merge_complete = false;
                break 'outer;
            }
        }
    }
    let complete = host_complete && snapshot.redactions_complete() && merge_complete;
    (collector.values(), complete)
}
