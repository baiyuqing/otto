//! Workflow definition catalog, parsing and validation.

use super::*;
use otto_platform::hex::to_hex;

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub(super) definitions: Vec<Definition>,
    pub(super) diagnostics: Vec<String>,
}

impl Catalog {
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_definitions(definitions: Vec<Definition>) -> Self {
        Self {
            definitions,
            diagnostics: Vec::new(),
        }
    }

    pub fn discover(
        roots: &[std::path::PathBuf],
        agents: &crate::subagent::Catalog,
    ) -> (Self, Vec<String>) {
        let mut by_name = std::collections::BTreeMap::new();
        let mut warnings = Vec::new();
        for root in roots {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!("workflows root {}: {error}", root.display()));
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                    continue;
                }
                match entry.file_type() {
                    Ok(file_type) if file_type.is_file() => {}
                    _ => continue,
                }
                let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let definition_bytes = match read_definition_file(&path) {
                    Ok(definition_bytes) => definition_bytes,
                    Err(error) => {
                        warnings.push(format!("workflow {}: {error}", path.display()));
                        continue;
                    }
                };
                match parse_definition(name, &definition_bytes, &|agent| {
                    agents.lookup(agent).is_some()
                }) {
                    Ok(mut definition) => {
                        definition.source = path.display().to_string();
                        let names: HashSet<String> = definition
                            .steps
                            .iter()
                            .filter(|step| matches!(step.kind, StepKind::Agent | StepKind::Handoff))
                            .map(|step| step.agent.clone())
                            .collect();
                        definition.agents = names
                            .into_iter()
                            .filter_map(|name| agents.lookup(&name))
                            .map(|agent| AgentSnapshot {
                                name: agent.name.clone(),
                                description: agent.description.clone(),
                                tools: agent.tools.clone(),
                                model: agent.model.clone(),
                                context: agent.context.clone(),
                                write_policy: SnapshotWritePolicy::from_subagent(
                                    agent.write_policy,
                                ),
                                write_paths: agent.write_paths.clone(),
                                body: agent.body.clone(),
                            })
                            .collect();
                        definition
                            .agents
                            .sort_by(|left, right| left.name.cmp(&right.name));
                        if let Err(error) = validate_write_coordination(&definition) {
                            warnings.push(format!("workflow {}: {error}", path.display()));
                            continue;
                        }
                        definition.hash.clear();
                        definition.hash = to_hex(&Sha256::digest(
                            serde_json::to_vec(&definition).unwrap_or_default(),
                        ));
                        by_name.insert(definition.name.clone(), definition);
                    }
                    Err(error) => warnings.push(format!("workflow {}: {error}", path.display())),
                }
            }
        }
        (
            Self {
                definitions: by_name.into_values().collect(),
                diagnostics: warnings.clone(),
            },
            warnings,
        )
    }

    pub fn lookup(&self, name: &str) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.name == name)
    }

    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }
}

pub(crate) fn validate_write_coordination(definition: &Definition) -> Result<(), Error> {
    let agents: HashMap<&str, &AgentSnapshot> = definition
        .agents
        .iter()
        .map(|agent| (agent.name.as_str(), agent))
        .collect();
    for (left_index, left) in definition.steps.iter().enumerate() {
        if left.kind != StepKind::Agent {
            continue;
        }
        let Some(left_agent) = agents.get(left.agent.as_str()) else {
            continue;
        };
        let Some(left_writer) = writer_scope(left_agent) else {
            continue;
        };
        for right in definition.steps.iter().skip(left_index + 1) {
            if right.kind != StepKind::Agent || ordered_by_dependency(definition, left, right) {
                continue;
            }
            let Some(right_agent) = agents.get(right.agent.as_str()) else {
                continue;
            };
            let Some(right_writer) = writer_scope(right_agent) else {
                continue;
            };
            if writer_scopes_conflict(&left_writer, &right_writer) {
                return Err(Error::invalid(format!(
                    "steps {:?} and {:?} may run concurrently and both can write; add a dependency or use disjoint owned_paths write policies",
                    left.id, right.id
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum WriterScope<'a> {
    Global,
    Owned(&'a [String]),
}

fn writer_scope(agent: &AgentSnapshot) -> Option<WriterScope<'_>> {
    if !agent_can_mutate(agent) {
        return None;
    }
    match agent.write_policy {
        SnapshotWritePolicy::ReadOnly | SnapshotWritePolicy::ProposeOnly => None,
        SnapshotWritePolicy::SingleWriter => Some(WriterScope::Global),
        SnapshotWritePolicy::OwnedPaths => Some(WriterScope::Owned(&agent.write_paths)),
    }
}

fn agent_can_mutate(agent: &AgentSnapshot) -> bool {
    agent.tools.as_ref().is_none_or(|tools| {
        tools
            .iter()
            .any(|tool| matches!(tool.as_str(), "write" | "edit"))
    })
}

fn writer_scopes_conflict(left: &WriterScope<'_>, right: &WriterScope<'_>) -> bool {
    match (left, right) {
        (WriterScope::Global, _) | (_, WriterScope::Global) => true,
        (WriterScope::Owned(left), WriterScope::Owned(right)) => left.iter().any(|left| {
            right
                .iter()
                .any(|right| ownership_patterns_overlap(left, right))
        }),
    }
}

fn ownership_patterns_overlap(left: &str, right: &str) -> bool {
    left == right
        || pattern_prefix(left).is_some_and(|prefix| right.starts_with(prefix))
        || pattern_prefix(right).is_some_and(|prefix| left.starts_with(prefix))
}

fn pattern_prefix(pattern: &str) -> Option<&str> {
    pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("/*"))
        .or_else(|| pattern.split_once('*').map(|(prefix, _)| prefix))
}

fn ordered_by_dependency(definition: &Definition, left: &Step, right: &Step) -> bool {
    depends_on(definition, &left.id, &right.id) || depends_on(definition, &right.id, &left.id)
}

fn depends_on(definition: &Definition, step_id: &str, dependency_id: &str) -> bool {
    let by_id: HashMap<&str, &Step> = definition
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect();
    let mut stack = vec![step_id];
    let mut seen = HashSet::new();
    while let Some(current) = stack.pop() {
        let Some(step) = by_id.get(current) else {
            continue;
        };
        for dependency in &step.needs {
            if dependency == dependency_id {
                return true;
            }
            if seen.insert(dependency.as_str()) {
                stack.push(dependency);
            }
        }
    }
    false
}

pub(super) fn read_definition_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if file.metadata()?.len() > MAX_DEFINITION_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds 1 MiB",
        ));
    }
    let mut definition_bytes = Vec::new();
    file.by_ref()
        .take((MAX_DEFINITION_BYTES + 1) as u64)
        .read_to_end(&mut definition_bytes)?;
    if definition_bytes.len() > MAX_DEFINITION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds 1 MiB",
        ));
    }
    Ok(definition_bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionFile {
    version: u32,
    #[serde(default)]
    description: String,
    steps: Vec<StepFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    id: String,
    #[serde(default)]
    kind: StepKind,
    #[serde(default)]
    agent: String,
    prompt: String,
    #[serde(default)]
    needs: Vec<String>,
}

pub(crate) fn parse_definition(
    name: &str,
    data: &[u8],
    has_agent: &dyn Fn(&str) -> bool,
) -> Result<Definition, Error> {
    if !valid_name(name) {
        return Err(Error::invalid(format!("workflow name {name:?} is invalid")));
    }
    if data.len() > MAX_DEFINITION_BYTES {
        return Err(Error::invalid(format!("workflow {name} exceeds 1 MiB")));
    }
    let text = std::str::from_utf8(data)
        .map_err(|_| Error::invalid(format!("workflow {name} is not UTF-8")))?;
    let file: DefinitionFile = toml::from_str(text)
        .map_err(|error| Error::invalid(format!("workflow {name}: {error}")))?;
    if file.version != 1 {
        return Err(Error::invalid(format!("workflow {name} version must be 1")));
    }
    if file.description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(Error::invalid(format!(
            "workflow {name} description exceeds {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    if !(1..=MAX_STEPS).contains(&file.steps.len()) {
        return Err(Error::invalid(format!(
            "workflow {name} must contain 1 to {MAX_STEPS} steps"
        )));
    }

    let mut seen = HashSet::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for raw in file.steps {
        if !valid_name(&raw.id) {
            return Err(Error::invalid(format!(
                "workflow {name} step id {:?} is invalid",
                raw.id
            )));
        }
        if !seen.insert(raw.id.clone()) {
            return Err(Error::invalid(format!(
                "workflow {name} contains duplicate step {:?}",
                raw.id
            )));
        }
        if raw.prompt.trim().is_empty() || raw.prompt.len() > MAX_TEXT_BYTES {
            return Err(Error::invalid(format!(
                "workflow {name} step {:?} prompt must be 1 to 65536 UTF-8 bytes",
                raw.id
            )));
        }
        match raw.kind {
            StepKind::Agent | StepKind::Handoff => {
                if raw.agent.is_empty() || !has_agent(&raw.agent) {
                    return Err(Error::invalid(format!(
                        "workflow {name} step {:?} references unknown agent {:?}",
                        raw.id, raw.agent
                    )));
                }
                if raw.kind == StepKind::Handoff && raw.needs.is_empty() {
                    return Err(Error::invalid(format!(
                        "workflow {name} handoff step {:?} requires a dependency",
                        raw.id
                    )));
                }
            }
            StepKind::Approval if !raw.agent.is_empty() => {
                return Err(Error::invalid(format!(
                    "workflow {name} approval step {:?} cannot set agent",
                    raw.id
                )));
            }
            StepKind::Approval => {}
        }
        let mut dependencies = HashSet::new();
        if raw
            .needs
            .iter()
            .any(|dependency| dependency == &raw.id || !dependencies.insert(dependency.clone()))
        {
            return Err(Error::invalid(format!(
                "workflow {name} step {:?} has an invalid dependency",
                raw.id
            )));
        }
        steps.push(Step {
            id: raw.id,
            kind: raw.kind,
            agent: raw.agent,
            prompt: raw.prompt,
            needs: raw.needs,
        });
    }
    validate_graph(name, &steps)?;

    Ok(Definition {
        name: name.to_string(),
        description: file.description,
        source: String::new(),
        steps,
        agents: Vec::new(),
        hash: to_hex(&Sha256::digest(data)),
    })
}

fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn validate_graph(name: &str, steps: &[Step]) -> Result<(), Error> {
    let indexes: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(index, step)| (step.id.as_str(), index))
        .collect();
    let mut incoming = vec![0usize; steps.len()];
    let mut outgoing = vec![Vec::new(); steps.len()];
    for (index, step) in steps.iter().enumerate() {
        for dependency in &step.needs {
            let Some(&source) = indexes.get(dependency.as_str()) else {
                return Err(Error::invalid(format!(
                    "workflow {name} step {:?} depends on unknown step {:?}",
                    step.id, dependency
                )));
            };
            incoming[index] += 1;
            outgoing[source].push(index);
        }
    }
    let mut ready: VecDeque<usize> = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for &target in &outgoing[index] {
            incoming[target] -= 1;
            if incoming[target] == 0 {
                ready.push_back(target);
            }
        }
    }
    if visited != steps.len() {
        return Err(Error::invalid(format!(
            "workflow {name} contains a dependency cycle"
        )));
    }
    Ok(())
}
