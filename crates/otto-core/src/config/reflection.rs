//! The `[reflection]` table and its resolution.
//!
//! Top-level only: reflection reads one session's transcript and writes to
//! the user's memory store, so a per-profile or per-project override would
//! change what is learned from a session depending on where it ran. See
//! `docs/specs/2026-10-02-session-reflection.md`.

use serde::{Deserialize, Serialize};

use super::ConfigError;

/// The smallest accepted `max_input_bytes`; below it a slice could not hold
/// one useful exchange.
pub const MINIMUM_INPUT_BYTES: usize = 1024;
/// The largest accepted `max_input_bytes`.
pub const MAXIMUM_INPUT_BYTES: usize = 4 * 1024 * 1024;
/// The largest accepted `max_memories`; the memory store accepts at most this
/// many candidates in one batch.
pub const MAXIMUM_MEMORIES: usize = 8;

/// The `[reflection]` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reflection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub memories: bool,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: usize,
    #[serde(default = "default_max_memories")]
    pub max_memories: usize,
}

impl Default for Reflection {
    fn default() -> Self {
        Reflection {
            enabled: true,
            memories: true,
            max_input_bytes: default_max_input_bytes(),
            max_memories: default_max_memories(),
        }
    }
}

impl Reflection {
    /// Whether the table equals its defaults, so a written config omits it.
    pub fn is_default(&self) -> bool {
        *self == Reflection::default()
    }
}

fn default_true() -> bool {
    true
}

fn default_max_input_bytes() -> usize {
    200 * 1024
}

fn default_max_memories() -> usize {
    MAXIMUM_MEMORIES
}

/// The resolved `[reflection]` configuration for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionRuntime {
    pub enabled: bool,
    pub memories: bool,
    pub max_input_bytes: usize,
    pub max_memories: usize,
}

impl Default for ReflectionRuntime {
    fn default() -> Self {
        let table = Reflection::default();
        ReflectionRuntime {
            enabled: table.enabled,
            memories: table.memories,
            max_input_bytes: table.max_input_bytes,
            max_memories: table.max_memories,
        }
    }
}

/// Resolves `[reflection]`, rejecting a bound outside its documented range and
/// naming the key.
pub fn resolve_reflection(file: &super::File) -> Result<ReflectionRuntime, ConfigError> {
    let table = &file.reflection;
    if !(MINIMUM_INPUT_BYTES..=MAXIMUM_INPUT_BYTES).contains(&table.max_input_bytes) {
        return Err(ConfigError::new(format!(
            "invalid reflection.max_input_bytes: must be between {MINIMUM_INPUT_BYTES} and {MAXIMUM_INPUT_BYTES}"
        )));
    }
    if table.max_memories == 0 || table.max_memories > MAXIMUM_MEMORIES {
        return Err(ConfigError::new(format!(
            "invalid reflection.max_memories: must be between 1 and {MAXIMUM_MEMORIES}"
        )));
    }
    Ok(ReflectionRuntime {
        enabled: table.enabled,
        memories: table.memories,
        max_input_bytes: table.max_input_bytes,
        max_memories: table.max_memories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{File, parse};

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn defaults_enable_reflection_with_bounded_input() {
        let runtime = resolve_reflection(&File::default()).expect("resolve");
        assert_eq!(
            runtime,
            ReflectionRuntime {
                enabled: true,
                memories: true,
                max_input_bytes: 204_800,
                max_memories: 8,
            }
        );
        assert!(File::default().reflection.is_default());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn explicit_table_is_used() {
        let file = parse(
            "[reflection]\nenabled = false\nmemories = false\nmax_input_bytes = 4096\nmax_memories = 3\n",
        )
        .expect("parse");
        let runtime = resolve_reflection(&file).expect("resolve");
        assert!(!runtime.enabled);
        assert!(!runtime.memories);
        assert_eq!(runtime.max_input_bytes, 4096);
        assert_eq!(runtime.max_memories, 3);
        assert!(!file.reflection.is_default());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn out_of_range_bounds_name_the_key() {
        for (text, key) in [
            ("[reflection]\nmax_input_bytes = 10\n", "max_input_bytes"),
            (
                "[reflection]\nmax_input_bytes = 99999999\n",
                "max_input_bytes",
            ),
            ("[reflection]\nmax_memories = 0\n", "max_memories"),
            ("[reflection]\nmax_memories = 9\n", "max_memories"),
        ] {
            let error = resolve_reflection(&parse(text).expect("parse")).expect_err(text);
            assert!(
                error.to_string().contains(&format!("reflection.{key}")),
                "{text}: {error}"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn unknown_keys_are_rejected() {
        assert!(parse("[reflection]\nauto = \"off\"\n").is_err());
    }
}
