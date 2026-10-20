//! `/reflect` command parsing and run-log reports.

use super::*;

/// What `/reflect <args>` asks for. `list` and `show <id>` read run logs;
/// any other text is the focus of a new run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReflectCommand<'a> {
    Run { focus: &'a str },
    List,
    Show { id: &'a str },
}

pub(crate) const REFLECT_USAGE: &str =
    "usage: /reflect [focus] | /reflect list | /reflect show <id>";

pub(crate) fn reflect_command(args: &str) -> ReflectCommand<'_> {
    let args = args.trim();
    let mut fields = args.split_whitespace();
    match (fields.next(), fields.next(), fields.next()) {
        (Some("list"), None, _) => ReflectCommand::List,
        (Some("show"), id, None) => ReflectCommand::Show {
            id: id.unwrap_or_default(),
        },
        _ => ReflectCommand::Run { focus: args },
    }
}

/// `/reflect list`, from the runs or the error text.
pub(crate) fn reflection_list_report(
    runs: Result<Vec<crate::reflection::runlog::Summary>, String>,
) -> String {
    match runs {
        Ok(runs) => crate::reflection::runlog::list_text(&runs),
        Err(error) => format!("/reflect list: {error}"),
    }
}

/// `/reflect show <id>`, from the log or the error text.
pub(crate) fn reflection_show_report(
    id: &str,
    log: Result<Option<(std::path::PathBuf, String)>, String>,
) -> String {
    if id.is_empty() {
        return REFLECT_USAGE.to_string();
    }
    match log {
        Ok(Some((path, content))) => {
            format!("Run log: {}\n\n{}", path.display(), content.trim_end())
        }
        Ok(None) => format!("No reflection run {id}; /reflect list shows the recent ones."),
        Err(error) => format!("/reflect show: {error}"),
    }
}

/// The text of `/reflect list` or `/reflect show <id>` for a local
/// controller; `None` for a new run, which the caller runs itself.
pub(crate) fn reflect_runs_report(controller: &Controller, args: &str) -> Option<String> {
    match reflect_command(args) {
        ReflectCommand::Run { .. } => None,
        ReflectCommand::List => Some(management_reflect_report(controller, "list", "")),
        ReflectCommand::Show { id } => Some(management_reflect_report(controller, "show", id)),
    }
}

fn management_reflect_report(controller: &Controller, command: &str, args: &str) -> String {
    use crate::app::management::{Reply, Request};
    let request = match Request::parse("reflect", &format!("{command} {args}")) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match controller.management(request) {
        Ok(Reply::ReflectList(rows)) => crate::reflection::runlog::list_text(&rows),
        Ok(reply @ Reply::ReflectShow { .. }) => format_management_reply(&reply),
        Err(error) => format!("/reflect {command}: {error}"),
        _ => unreachable!("reflection request returned a different management reply"),
    }
}
