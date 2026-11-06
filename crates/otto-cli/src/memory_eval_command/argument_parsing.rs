//! Argument grammar of `otto eval memory-retraction` and the `--dry-run` estimate.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which part of the evaluation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Two,
    Three,
    All,
}

impl Layer {
    pub fn includes(self, layer: Layer) -> bool {
        self == Layer::All || self == layer
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Two => "2",
            Layer::Three => "3",
            Layer::All => "all",
        }
    }

    pub fn parse(value: &str) -> Option<Layer> {
        match value {
            "2" => Some(Layer::Two),
            "3" => Some(Layer::Three),
            "all" => Some(Layer::All),
            _ => None,
        }
    }
}

/// The context conditions of #591 section 10 layer 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Condition {
    /// The obsolete record is active and recalled.
    A,
    /// The record is not recalled and its lifecycle is unknown: nothing is filtered or
    /// announced; copies in the transcript remain.
    B,
    /// The lifecycle reports the retirement: the notice renders and the replacement is
    /// recalled where one exists.
    C,
    /// C, and the checkpoint summary is rebuilt before the task turn.
    D,
}

impl Condition {
    pub const ALL: [Condition; 4] = [Condition::A, Condition::B, Condition::C, Condition::D];

    pub fn as_str(self) -> &'static str {
        match self {
            Condition::A => "A",
            Condition::B => "B",
            Condition::C => "C",
            Condition::D => "D",
        }
    }

    pub fn parse(value: &str) -> Option<Condition> {
        Condition::ALL
            .into_iter()
            .find(|condition| condition.as_str().eq_ignore_ascii_case(value))
    }
}

/// Where the obsolete claim sits in the transcript before the task turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    /// A prior turn's `otto.memory_recall` entry references the record; no copy in the text.
    NativeRecall,
    /// The reference plus an assistant message that restates the claim.
    AssistantParaphrase,
    /// A tool result holds the claim; no reference.
    ToolResult,
    /// A checkpoint summary holds the claim and `memoryRecords` names the record.
    SummaryWithReferences,
    /// The same summary without `memoryRecords`.
    LegacySummary,
}

impl Variant {
    pub const ALL: [Variant; 5] = [
        Variant::NativeRecall,
        Variant::AssistantParaphrase,
        Variant::ToolResult,
        Variant::SummaryWithReferences,
        Variant::LegacySummary,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Variant::NativeRecall => "native_recall",
            Variant::AssistantParaphrase => "assistant_paraphrase",
            Variant::ToolResult => "tool_result",
            Variant::SummaryWithReferences => "summary_with_references",
            Variant::LegacySummary => "legacy_summary",
        }
    }

    pub fn parse(value: &str) -> Option<Variant> {
        Variant::ALL
            .into_iter()
            .find(|variant| variant.as_str() == value)
    }

    /// Whether the session carries a reference to the record, so a notice can select it.
    pub fn traceable(self) -> bool {
        matches!(
            self,
            Variant::NativeRecall | Variant::AssistantParaphrase | Variant::SummaryWithReferences
        )
    }

    /// Whether the transcript holds a compaction checkpoint, so condition D applies.
    pub fn has_summary(self) -> bool {
        matches!(
            self,
            Variant::SummaryWithReferences | Variant::LegacySummary
        )
    }
}

/// Every flag value, with defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub layer: Layer,
    pub conditions: Vec<Condition>,
    pub variants: Vec<Variant>,
    /// Item names to run; `None` runs every item.
    pub items: Option<Vec<String>>,
    pub repeat: u32,
    pub out: Option<PathBuf>,
    pub max_calls: u32,
    /// Whether the LLM judge runs; `--no-judge` clears it.
    pub judge: bool,
    pub dry_run: bool,
    pub cwd: String,
    pub config: Option<String>,
}

pub const DEFAULT_MAX_CALLS: u32 = 2_000;

impl Default for Options {
    fn default() -> Self {
        Self {
            layer: Layer::All,
            conditions: Condition::ALL.to_vec(),
            variants: Variant::ALL.to_vec(),
            items: None,
            repeat: 1,
            out: None,
            max_calls: DEFAULT_MAX_CALLS,
            judge: true,
            dry_run: false,
            cwd: ".".into(),
            config: None,
        }
    }
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub options: Options,
}

impl Invocation {
    pub fn is_dry_run(&self) -> bool {
        self.options.dry_run
    }

    /// The flags the shared option parser needs to locate the workspace and configuration.
    pub fn startup_flags(&self) -> Vec<String> {
        let mut flags = vec!["--cwd".to_string(), self.options.cwd.clone()];
        if let Some(config) = &self.options.config {
            flags.push("--config".into());
            flags.push(config.clone());
        }
        flags
    }
}

/// Parses the arguments after `memory-retraction`.
pub fn parse(_args: &[String]) -> Result<Invocation, String> {
    Err("not implemented".into())
}

/// Prints the dry-run estimate: items and labeled items, cases per layer, provider calls per
/// layer and in total, and the input byte estimate.
pub fn dry_run(_options: &Options, _stdout: &mut (dyn Write + Send)) -> Result<(), String> {
    Err("not implemented".into())
}
