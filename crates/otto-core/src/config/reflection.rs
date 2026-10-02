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
/// The largest accepted `max_skills` (per run) and `max_generated_skills`
/// (in total).
pub const MAXIMUM_SKILLS_PER_RUN: usize = 8;
pub const MAXIMUM_GENERATED_SKILLS: usize = 200;

/// The largest accepted `min_turns`.
pub const MAXIMUM_MIN_TURNS: usize = 1000;

/// When reflection runs by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auto {
    /// Only on `/reflect`.
    Off,
    /// Once when a terminal session closes normally, over what no earlier run
    /// covered, if it has at least `min_turns` user messages.
    OnExit,
    /// In the background after each successful compaction (the default).
    OnCompaction,
}

/// When reflection may write skills, given where the slice's content came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    /// Only when no entry in the slice came from outside the user and the
    /// workspace (the default).
    Untainted,
    /// Also when external entries are present; they are still withheld from
    /// the model and cannot be cited.
    Any,
}

/// The `[reflection]` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reflection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub memories: bool,
    #[serde(default = "default_auto")]
    pub auto: Auto,
    #[serde(default = "default_min_turns")]
    pub min_turns: usize,
    #[serde(default = "default_true")]
    pub skills: bool,
    #[serde(default = "default_skill_source")]
    pub skill_source: SkillSource,
    #[serde(default = "default_true")]
    pub skill_review: bool,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: usize,
    #[serde(default = "default_max_memories")]
    pub max_memories: usize,
    #[serde(default = "default_max_skills")]
    pub max_skills: usize,
    #[serde(default = "default_max_generated_skills")]
    pub max_generated_skills: usize,
}

impl Default for Reflection {
    fn default() -> Self {
        Reflection {
            enabled: true,
            memories: true,
            auto: default_auto(),
            min_turns: default_min_turns(),
            skills: true,
            skill_source: default_skill_source(),
            skill_review: true,
            max_input_bytes: default_max_input_bytes(),
            max_memories: default_max_memories(),
            max_skills: default_max_skills(),
            max_generated_skills: default_max_generated_skills(),
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

fn default_auto() -> Auto {
    Auto::OnCompaction
}

fn default_min_turns() -> usize {
    4
}

fn default_skill_source() -> SkillSource {
    SkillSource::Untainted
}

fn default_max_skills() -> usize {
    2
}

fn default_max_generated_skills() -> usize {
    30
}

/// The resolved `[reflection]` configuration for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionRuntime {
    pub enabled: bool,
    pub memories: bool,
    pub auto: Auto,
    pub min_turns: usize,
    pub skills: bool,
    pub skill_source: SkillSource,
    pub skill_review: bool,
    pub max_input_bytes: usize,
    pub max_memories: usize,
    pub max_skills: usize,
    pub max_generated_skills: usize,
}

impl Default for ReflectionRuntime {
    fn default() -> Self {
        let table = Reflection::default();
        ReflectionRuntime {
            enabled: table.enabled,
            memories: table.memories,
            auto: table.auto,
            min_turns: table.min_turns,
            skills: table.skills,
            skill_source: table.skill_source,
            skill_review: table.skill_review,
            max_input_bytes: table.max_input_bytes,
            max_memories: table.max_memories,
            max_skills: table.max_skills,
            max_generated_skills: table.max_generated_skills,
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
    if table.max_skills == 0 || table.max_skills > MAXIMUM_SKILLS_PER_RUN {
        return Err(ConfigError::new(format!(
            "invalid reflection.max_skills: must be between 1 and {MAXIMUM_SKILLS_PER_RUN}"
        )));
    }
    if table.max_generated_skills == 0 || table.max_generated_skills > MAXIMUM_GENERATED_SKILLS {
        return Err(ConfigError::new(format!(
            "invalid reflection.max_generated_skills: must be between 1 and {MAXIMUM_GENERATED_SKILLS}"
        )));
    }
    if table.min_turns == 0 || table.min_turns > MAXIMUM_MIN_TURNS {
        return Err(ConfigError::new(format!(
            "invalid reflection.min_turns: must be between 1 and {MAXIMUM_MIN_TURNS}"
        )));
    }
    Ok(ReflectionRuntime {
        enabled: table.enabled,
        memories: table.memories,
        auto: table.auto,
        min_turns: table.min_turns,
        skills: table.skills,
        skill_source: table.skill_source,
        skill_review: table.skill_review,
        max_input_bytes: table.max_input_bytes,
        max_memories: table.max_memories,
        max_skills: table.max_skills,
        max_generated_skills: table.max_generated_skills,
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
                auto: Auto::OnCompaction,
                min_turns: 4,
                skills: true,
                skill_source: SkillSource::Untainted,
                skill_review: true,
                max_input_bytes: 204_800,
                max_memories: 8,
                max_skills: 2,
                max_generated_skills: 30,
            }
        );
        assert!(File::default().reflection.is_default());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn explicit_table_is_used() {
        let file = parse(
            "[reflection]\nenabled = false\nmemories = false\nauto = \"on_exit\"\nmin_turns = 6\nskills = false\nskill_source = \"any\"\nskill_review = false\nmax_input_bytes = 4096\nmax_memories = 3\nmax_skills = 1\nmax_generated_skills = 5\n",
        )
        .expect("parse");
        let runtime = resolve_reflection(&file).expect("resolve");
        assert!(!runtime.enabled);
        assert!(!runtime.memories);
        assert_eq!(runtime.max_input_bytes, 4096);
        assert_eq!(runtime.max_memories, 3);
        assert_eq!((runtime.auto, runtime.min_turns), (Auto::OnExit, 6));
        assert!(!runtime.skills);
        assert_eq!(runtime.skill_source, SkillSource::Any);
        assert!(!runtime.skill_review);
        assert_eq!((runtime.max_skills, runtime.max_generated_skills), (1, 5));
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
            ("[reflection]\nmin_turns = 0\n", "min_turns"),
            ("[reflection]\nmin_turns = 1001\n", "min_turns"),
            ("[reflection]\nmax_skills = 0\n", "max_skills"),
            ("[reflection]\nmax_skills = 9\n", "max_skills"),
            (
                "[reflection]\nmax_generated_skills = 0\n",
                "max_generated_skills",
            ),
            (
                "[reflection]\nmax_generated_skills = 201\n",
                "max_generated_skills",
            ),
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
        assert!(parse("[reflection]\nauto = \"sometimes\"\n").is_err());
        assert!(parse("[reflection]\nskill_source = \"anywhere\"\n").is_err());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn every_auto_mode_parses() {
        for (text, mode) in [
            ("off", Auto::Off),
            ("on_exit", Auto::OnExit),
            ("on_compaction", Auto::OnCompaction),
        ] {
            let file = parse(&format!("[reflection]\nauto = \"{text}\"\n")).expect("parse");
            assert_eq!(resolve_reflection(&file).expect("resolve").auto, mode);
        }
    }
}
