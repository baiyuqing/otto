//! Task spec and task result transcript entries a child records.

use super::*;

/// The `customType` of the entry a child transcript gets as soon as its task
/// is created, before the child agent runs. See [`spawn`](Runner::spawn).
///
/// `pub(crate)` so [`crate::subagent::interrupted::scan`] can find this entry
/// type without duplicating the literal.
pub(crate) const TASK_SPEC_CUSTOM_TYPE: &str = "otto.task_spec";

/// The `customType` of the entry a child transcript gets when its task
/// reaches a terminal status. See [`finish`](Runner::finish).
///
/// `pub(crate)` so [`crate::subagent::interrupted`] can find and write this
/// entry type without duplicating the literal.
pub(crate) const TASK_RESULT_CUSTOM_TYPE: &str = "otto.task_result";

/// The JSON `data` of an `otto.task_spec` custom entry: the task as created,
/// and the agent definition as resolved at start (the same snapshot
/// [`Runner::run_with_definition_control`] uses), so a queued task's transcript
/// already records what it was asked to do before it starts.
///
/// `pub(crate)` and `Deserialize` so [`crate::subagent::interrupted::scan`]
/// reads back the exact type [`Runner::spawn`] writes, instead of a
/// hand-copied twin. `prompt` defaults on deserialize so a transcript written
/// before this field existed still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskSpecData {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) model_source: super::super::tasks::ModelSource,
    pub(crate) context: String,
    #[serde(default)]
    pub(crate) prompt: String,
    pub(crate) definition: Option<TaskSpecDefinition>,
    /// The tool call of the parent session that started this task; empty for
    /// tasks started outside a tool call and for transcripts written before
    /// this field existed.
    #[serde(
        rename = "parentToolCallId",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub(crate) parent_tool_call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskSpecDefinition {
    pub(crate) name: String,
    pub(crate) body: String,
    /// The definition's own tool allowlist, `None` for every child tool.
    /// Unresolved against the live registry, matching what
    /// [`Runner::run_with_definition_control`] snapshots.
    #[serde(default)]
    pub(crate) tools: Option<Vec<String>>,
    #[serde(rename = "writePolicy")]
    pub(crate) write_policy: String,
    #[serde(rename = "writePaths", default)]
    pub(crate) write_paths: Vec<String>,
}

impl TaskSpecDefinition {
    /// Rebuilds a [`Definition`] from this snapshot, for a resumed task: the
    /// fields a spawn actually reads (`name`, `body`, `tools`, `write_policy`,
    /// `write_paths`) come from the recording; `description`, `model`,
    /// `context`, `directory`, `path` and `is_skill_derived` are not part of
    /// the `otto.task_spec` snapshot and are left at their default.
    ///
    /// Fails when `write_policy` is not one of the recognized values, naming
    /// it in the error, rather than silently falling back to a default
    /// policy for a task whose recorded write coordination cannot be
    /// honored.
    pub(crate) fn to_definition(&self) -> Result<Definition, String> {
        let write_policy = self.write_policy.parse().map_err(|error: String| {
            format!(
                "recorded task_spec has write_policy {:?}: {error}",
                self.write_policy
            )
        })?;
        Ok(Definition {
            name: self.name.clone(),
            tools: self.tools.clone(),
            write_policy,
            write_paths: self.write_paths.clone(),
            body: self.body.clone(),
            ..Definition::default()
        })
    }
}

/// The JSON `data` of an `otto.task_result` custom entry.
///
/// `pub(crate)` so [`crate::subagent::interrupted::mark_interrupted`] writes
/// the same shape [`Runner::finish`] does.
#[derive(Debug, Serialize)]
pub(crate) struct TaskResultData<'a> {
    pub(crate) status: &'a str,
    pub(crate) error: &'a str,
}
