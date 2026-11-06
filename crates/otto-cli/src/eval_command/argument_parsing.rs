//! Argument grammar of `otto eval view-tree <build|probes|run|all>`.

use std::path::PathBuf;

use otto_core::view_tree::CachePrices;

/// The four stages of the evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Build,
    Probes,
    Run,
    All,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Probes => "probes",
            Self::Run => "run",
            Self::All => "all",
        }
    }
}

/// Every flag value, with defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalOptions {
    pub session: Option<String>,
    pub out: Option<PathBuf>,
    pub model: Option<String>,
    pub view_low_kb: usize,
    pub view_high_kb: usize,
    pub concurrency: usize,
    pub dry_run: bool,
    pub probe_count: usize,
    pub probes_file: Option<PathBuf>,
    pub max_steps: u32,
    pub prices: CachePrices,
    pub cwd: String,
    pub config: Option<String>,
}

impl Default for EvalOptions {
    fn default() -> Self {
        Self {
            session: None,
            out: None,
            model: None,
            view_low_kb: 64,
            view_high_kb: 128,
            concurrency: 8,
            dry_run: false,
            probe_count: 20,
            probes_file: None,
            max_steps: 12,
            prices: CachePrices::default(),
            cwd: ".".into(),
            config: None,
        }
    }
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub action: Action,
    pub options: EvalOptions,
}

impl Invocation {
    /// Whether the command only prints the size of the build.
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

const BUILD_AND_ALL: &[Action] = &[Action::Build, Action::All];
const EVERY_ACTION: &[Action] = &[Action::Build, Action::Probes, Action::Run, Action::All];

/// Parses the arguments after `eval`.
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let Some(target) = args.first() else {
        return Err("otto: eval requires a target: view-tree or memory-retraction".into());
    };
    if target != "view-tree" {
        return Err(format!(
            "otto: unknown eval target {target:?}; the targets are view-tree and memory-retraction"
        ));
    }
    let action = match args.get(1).map(String::as_str) {
        Some("build") => Action::Build,
        Some("probes") => Action::Probes,
        Some("run") => Action::Run,
        Some("all") => Action::All,
        Some(other) => {
            return Err(format!(
                "otto: unknown eval view-tree action {other:?}; use build, probes, run or all"
            ));
        }
        None => {
            return Err(
                "otto: eval view-tree requires an action: build, probes, run or all".into(),
            );
        }
    };
    let mut options = EvalOptions::default();
    let mut index = 2;
    while index < args.len() {
        let flag = args[index].as_str();
        let allowed =
            flag_actions(flag).ok_or_else(|| format!("otto: unknown eval flag {flag:?}"))?;
        if !allowed.contains(&action) {
            return Err(format!(
                "otto: flag {flag} does not apply to eval view-tree {}",
                action.name()
            ));
        }
        if flag == "--dry-run" {
            options.dry_run = true;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("otto: flag {flag} requires a value"))?;
        apply_flag(&mut options, flag, value)?;
        index += 2;
    }
    require(&options, action)?;
    Ok(Invocation { action, options })
}

fn flag_actions(flag: &str) -> Option<&'static [Action]> {
    Some(match flag {
        "--session" | "--model" | "--view-low" | "--view-high" | "--concurrency" | "--dry-run"
        | "--cache-read" | "--cache-write" | "--output-price" | "--compressor-ratio"
        | "--bytes-per-token" => BUILD_AND_ALL,
        "--count" => &[Action::Probes, Action::All],
        "--probes" | "--max-steps" => &[Action::Run, Action::All],
        "--out" | "--cwd" | "--config" => EVERY_ACTION,
        _ => return None,
    })
}

fn apply_flag(options: &mut EvalOptions, flag: &str, value: &str) -> Result<(), String> {
    match flag {
        "--session" => options.session = Some(non_empty(flag, value)?),
        "--out" => options.out = Some(PathBuf::from(non_empty(flag, value)?)),
        "--model" => options.model = Some(non_empty(flag, value)?),
        "--probes" => options.probes_file = Some(PathBuf::from(non_empty(flag, value)?)),
        "--cwd" => options.cwd = non_empty(flag, value)?,
        "--config" => options.config = Some(non_empty(flag, value)?),
        "--view-low" => options.view_low_kb = positive_integer(flag, value)?,
        "--view-high" => options.view_high_kb = positive_integer(flag, value)?,
        "--concurrency" => options.concurrency = positive_integer(flag, value)?,
        "--count" => options.probe_count = positive_integer(flag, value)?,
        "--max-steps" => {
            options.max_steps =
                u32::try_from(positive_integer(flag, value)?).map_err(|_| too_large(flag))?
        }
        "--cache-read" => options.prices.cache_read = positive_number(flag, value)?,
        "--cache-write" => options.prices.cache_write = positive_number(flag, value)?,
        "--output-price" => options.prices.output = positive_number(flag, value)?,
        "--compressor-ratio" => options.prices.compressor_ratio = positive_number(flag, value)?,
        "--bytes-per-token" => options.prices.bytes_per_token = positive_number(flag, value)?,
        _ => return Err(format!("otto: unknown eval flag {flag:?}")),
    }
    Ok(())
}

fn too_large(flag: &str) -> String {
    format!("otto: flag {flag} is too large")
}

fn non_empty(flag: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("otto: flag {flag} requires a non-empty value"));
    }
    Ok(value.to_string())
}

fn positive_integer(flag: &str, value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(number) if number >= 1 => Ok(number),
        _ => Err(format!(
            "otto: flag {flag} requires an integer of at least 1; got {value:?}"
        )),
    }
}

fn positive_number(flag: &str, value: &str) -> Result<f64, String> {
    match value.parse::<f64>() {
        Ok(number) if number.is_finite() && number > 0.0 => Ok(number),
        _ => Err(format!(
            "otto: flag {flag} requires a number above 0; got {value:?}"
        )),
    }
}

fn require(options: &EvalOptions, action: Action) -> Result<(), String> {
    if matches!(action, Action::Build | Action::All) && options.session.is_none() {
        return Err(format!(
            "otto: eval view-tree {} requires --session <id|path>",
            action.name()
        ));
    }
    if matches!(action, Action::Probes | Action::Run) && options.out.is_none() {
        return Err(format!(
            "otto: eval view-tree {} requires --out DIR",
            action.name()
        ));
    }
    if options.view_low_kb > options.view_high_kb {
        return Err(format!(
            "otto: --view-low ({}) must not exceed --view-high ({})",
            options.view_low_kb, options.view_high_kb
        ));
    }
    Ok(())
}
