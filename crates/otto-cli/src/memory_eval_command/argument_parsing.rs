//! Argument grammar of `otto eval memory-retraction` and the `--dry-run` estimate.

use std::io::Write;
use std::path::PathBuf;

use otto::memory::proposals::evaluation::LabeledItem;
use serde::{Deserialize, Serialize};

use super::conditions::{self, AGENT_SYSTEM_PROMPT};
use super::items::Item;

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

const COMMAND: &str = "otto: eval memory-retraction:";
const MAX_REPEAT: u32 = 20;
const MAX_CALLS_LIMIT: u32 = 100_000;

/// Parses the arguments after `memory-retraction`.
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut options = Options::default();
    let mut seen: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if !is_known_flag(flag) {
            return Err(format!("{COMMAND} unknown flag {flag:?}"));
        }
        if seen.contains(&flag) {
            return Err(format!("{COMMAND} flag {flag} is given more than once"));
        }
        seen.push(flag);
        if flag == "--no-judge" || flag == "--dry-run" {
            if flag == "--no-judge" {
                options.judge = false;
            } else {
                options.dry_run = true;
            }
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{COMMAND} flag {flag} requires a value"))?;
        apply_flag(&mut options, flag, value)?;
        index += 2;
    }
    Ok(Invocation { options })
}

fn is_known_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--layer"
            | "--conditions"
            | "--variants"
            | "--items"
            | "--repeat"
            | "--out"
            | "--max-calls"
            | "--no-judge"
            | "--dry-run"
            | "--cwd"
            | "--config"
    )
}

fn apply_flag(options: &mut Options, flag: &str, value: &str) -> Result<(), String> {
    match flag {
        "--layer" => {
            options.layer = Layer::parse(value).ok_or_else(|| {
                format!("{COMMAND} flag {flag} requires 2, 3 or all; got {value:?}")
            })?;
        }
        "--conditions" => {
            options.conditions = parse_list(flag, value, Condition::parse)?;
        }
        "--variants" => {
            options.variants = parse_list(flag, value, Variant::parse)?;
        }
        "--items" => {
            options.items = Some(parse_list(flag, value, |entry| Some(entry.to_owned()))?);
        }
        "--repeat" => options.repeat = bounded_integer(flag, value, MAX_REPEAT)?,
        "--max-calls" => options.max_calls = bounded_integer(flag, value, MAX_CALLS_LIMIT)?,
        "--out" => options.out = Some(PathBuf::from(non_empty(flag, value)?)),
        "--cwd" => options.cwd = non_empty(flag, value)?,
        "--config" => options.config = Some(non_empty(flag, value)?),
        _ => return Err(format!("{COMMAND} unknown flag {flag:?}")),
    }
    Ok(())
}

fn non_empty(flag: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{COMMAND} flag {flag} requires a non-empty value"));
    }
    Ok(value.to_owned())
}

fn bounded_integer(flag: &str, value: &str, maximum: u32) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(number) if (1..=maximum).contains(&number) => Ok(number),
        _ => Err(format!(
            "{COMMAND} flag {flag} requires an integer from 1 to {maximum}; got {value:?}"
        )),
    }
}

/// Parses a comma list into unique entries in the given order. `convert` maps an entry to its
/// value or `None` when the entry is not valid for the flag.
fn parse_list<T: PartialEq>(
    flag: &str,
    value: &str,
    convert: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, String> {
    let mut entries: Vec<T> = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(format!(
                "{COMMAND} flag {flag} has an empty list entry in {value:?}"
            ));
        }
        let converted = convert(entry)
            .ok_or_else(|| format!("{COMMAND} flag {flag} has an unknown entry {entry:?}"))?;
        if entries.contains(&converted) {
            return Err(format!(
                "{COMMAND} flag {flag} lists {entry:?} more than once"
            ));
        }
        entries.push(converted);
    }
    Ok(entries)
}

/// Prints the dry-run estimate: items and labeled items, cases per layer, provider calls per
/// layer and in total, and the input byte estimate. It loads the embedded items and the
/// labeled dataset; a load failure is returned.
pub fn dry_run(options: &Options, stdout: &mut (dyn Write + Send)) -> Result<(), String> {
    let items = super::items::selected_items(options.items.as_deref())?;
    let labeled = super::layer_two::labeled_items()?;
    dry_run_items(&items, &labeled, options, stdout)
}

/// The estimate over given items; `dry_run` loads the embedded ones.
///
/// Layer 3 cases are items x variants x applicable conditions x repeat. Each case makes two
/// agent answers (task and control); judging adds one call per case; a case under condition D
/// adds one compaction call. Layer 2 makes two decision rounds per labeled item; judging adds
/// one call per labeled item. Layer 3 input bytes per case are the prior texts, the record,
/// replacement and control record texts, the task, the control task and the agent system
/// prompt, counted twice because the task and the control turn each send them. Layer 2 input
/// bytes are the record and evidence texts, counted twice for the two rounds.
pub(super) fn dry_run_items(
    items: &[Item],
    labeled: &[LabeledItem],
    options: &Options,
    stdout: &mut (dyn Write + Send),
) -> Result<(), String> {
    let mut cases = 0usize;
    let mut compactions = 0usize;
    let mut layer_three_bytes = 0usize;
    if options.layer.includes(Layer::Three) {
        for item in items {
            let per_case_bytes = (case_input_bytes(item) + AGENT_SYSTEM_PROMPT.len()) * 2;
            for variant in &options.variants {
                for condition in &options.conditions {
                    if !conditions::applicable(*variant, *condition) {
                        continue;
                    }
                    let item_cases = options.repeat as usize;
                    cases += item_cases;
                    layer_three_bytes += per_case_bytes * item_cases;
                    if *condition == Condition::D {
                        compactions += item_cases;
                    }
                }
            }
        }
    }
    let judged_answers = if options.judge { cases } else { 0 };
    let layer_three_calls = cases * 2 + judged_answers + compactions;
    let (layer_two_items, layer_two_bytes) = if options.layer.includes(Layer::Two) {
        let bytes = labeled.iter().map(labeled_input_bytes).sum::<usize>() * 2;
        (labeled.len(), bytes)
    } else {
        (0, 0)
    };
    let layer_two_calls = layer_two_items * 2 + if options.judge { layer_two_items } else { 0 };
    let lines = [
        format!("items: {}", items.len()),
        format!("labeled items: {}", labeled.len()),
        format!("layer 3 cases: {cases}"),
        format!("layer 3 provider calls (estimate): {layer_three_calls}"),
        format!("layer 2 provider calls (estimate): {layer_two_calls}"),
        format!(
            "provider calls total (estimate): {}",
            layer_three_calls + layer_two_calls
        ),
        format!(
            "task input bytes (estimate): {}",
            layer_three_bytes + layer_two_bytes
        ),
    ];
    for line in lines {
        writeln!(stdout, "{line}").map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Bytes of the texts a layer-3 case sends, without the agent system prompt.
fn case_input_bytes(item: &Item) -> usize {
    let prior = &item.prior;
    let prior_bytes = [
        &prior.user,
        &prior.assistant_neutral,
        &prior.assistant,
        &prior.tool_name,
        &prior.tool_input,
        &prior.tool_output,
        &prior.summary,
    ]
    .iter()
    .map(|text| text.len())
    .sum::<usize>();
    let replacement_bytes = item
        .replacement
        .as_ref()
        .map_or(0, |replacement| replacement.text.len());
    prior_bytes
        + item.record.text.len()
        + replacement_bytes
        + item.control.record.text.len()
        + item.task.len()
        + item.control.task.len()
}

fn labeled_input_bytes(item: &LabeledItem) -> usize {
    item.record.text.len()
        + item
            .evidence
            .iter()
            .map(|evidence| evidence.text.len())
            .sum::<usize>()
}
