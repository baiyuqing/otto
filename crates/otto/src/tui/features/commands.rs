use super::super::backend::{Command, Reply, Request};
use super::super::interaction::{Action, Activation, Field, View};
use super::approval::Approvals;
use super::choice::{Choice, ChoiceKind};
use super::collection::{Agents, Context};
use super::review::Reviews;
use super::{Document, Presenter, Run, Transition};
use crate::app::Info;

type Factory = fn(&str, &Info, &Catalog) -> Result<Transition, String>;

#[derive(Clone, Copy)]
pub(crate) struct Entry {
    pub name: &'static str,
    pub description: &'static str,
    pub local_only: bool,
    pub factory: Factory,
}

pub(crate) struct Catalog {
    entries: Vec<Entry>,
}

impl Default for Catalog {
    fn default() -> Self {
        {
            let mut catalog = Self {
                entries: Vec::new(),
            };
            for entry in ENTRIES {
                catalog.register(*entry).expect("unique built-in command");
            }
            catalog
        }
    }
}

impl Catalog {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn register(&mut self, entry: Entry) -> Result<(), String> {
        if self
            .entries
            .iter()
            .any(|existing| existing.name == entry.name)
        {
            return Err(format!("duplicate command: {}", entry.name));
        }
        self.entries.push(entry);
        Ok(())
    }
    pub fn open(&self, line: &str, info: &Info, attached: bool) -> Result<Transition, String> {
        let (mut name, args) = line
            .trim()
            .split_once(char::is_whitespace)
            .map(|(n, a)| (n, a.trim()))
            .unwrap_or((line.trim(), ""));
        if name == "/tasks" && args.is_empty() {
            name = "/task";
        } else if name == "/quit" {
            name = "/exit";
        }
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| format!("unknown command: {line}"))?;
        if attached && entry.local_only {
            return Err(format!("{name}: not available with --attach"));
        }
        if attached
            && name == "/sandbox"
            && matches!(
                args.split_whitespace().next(),
                Some("allow" | "network" | "exclude")
            )
        {
            return Err(format!("{name}: not available with --attach"));
        }
        (entry.factory)(args, info, self)
    }
}

macro_rules! entry {
    ($name:literal,$desc:literal,$local:expr,$factory:expr) => {
        Entry {
            name: $name,
            description: $desc,
            local_only: $local,
            factory: $factory,
        }
    };
}

const ENTRIES: &[Entry] = &[
    entry!(
        "/permissions",
        "inspect or set approval policy and reviewer",
        false,
        permissions
    ),
    entry!(
        "/auto",
        "show or set the approval mode: manual, auto or yolo",
        false,
        auto
    ),
    entry!("/help", "show commands", false, help),
    entry!("/init", "create a repository AGENTS.md guide", false, init),
    entry!("/session", "show session details", false, session),
    entry!("/plan", "show the session plan", false, plan),
    entry!("/new", "start a new session", false, new_session),
    entry!("/clear", "start a new session", false, new_session),
    entry!("/model", "switch profiles", true, model),
    entry!("/thinking", "set reasoning effort", true, thinking),
    entry!("/resume", "resume a session", false, resume),
    entry!("/fork", "fork the session before a prompt", false, fork),
    entry!("/archive", "archive a session", true, archive),
    entry!("/rename", "rename the current session", false, rename),
    entry!("/compact", "compact context", false, compact),
    entry!(
        "/reflect",
        "reflect, list runs, or show a run",
        false,
        reflect
    ),
    entry!("/image", "attach an image to the next prompt", false, image),
    entry!(
        "/sandbox",
        "inspect or change sandbox settings",
        false,
        sandbox
    ),
    entry!(
        "/approve",
        "open approvals, or allow one exact request",
        false,
        approve
    ),
    entry!("/steer", "guide the active task", false, steer),
    entry!(
        "/inputs",
        "list or withdraw guidance for the active task",
        false,
        inputs
    ),
    entry!(
        "/memory",
        "search, inspect, forget or review memories",
        true,
        memory
    ),
    entry!("/remember", "remember a record", true, remember),
    entry!("/login", "authenticate a provider", true, login),
    entry!("/logout", "remove provider credentials", true, logout),
    entry!("/exit", "exit Otto", false, exit),
    entry!("/task", "list, inspect or cancel tasks", true, task),
    entry!("/agents", "browse recorded agents", false, agents),
    entry!("/context", "inspect context", false, context),
    entry!("/trace", "show the turn timeline", false, trace),
    entry!("/timers", "list or cancel reminders", true, timers),
    entry!("/skill", "list, inspect or configure skills", true, skill),
    entry!("/mcp", "inspect MCP servers or authenticate one", true, mcp),
];

fn push(p: impl Presenter + 'static) -> Result<Transition, String> {
    Ok(Transition::Push(Box::new(p)))
}
fn empty(args: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err("unexpected arguments".into())
    }
}
fn query(title: &str, request: Request) -> Result<Transition, String> {
    let retry = (!matches!(request, Request::Command(_))).then(|| request.clone());
    push(RequestPresenter {
        retry,
        title: title.into(),
        request: Some(request),
        text: String::new(),
        loaded: false,
    })
}
fn command(title: &str, command: Command) -> Result<Transition, String> {
    query(title, Request::Command(command))
}
fn help(args: &str, _: &Info, catalog: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    push(Document {
        key: "help".into(),
        title: "Commands".into(),
        text: catalog
            .entries()
            .iter()
            .map(|e| format!("{}  {}", e.name, e.description))
            .collect::<Vec<_>>()
            .join("\n"),
    })
}
fn init(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    Ok(Transition::Run(Run::Prompt(
        otto_core::agent::INIT_PROMPT.into(),
    )))
}
fn exit(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    Ok(Transition::Run(Run::Exit))
}
fn new_session(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    command("New session", Command::NewSession)
}
fn permissions(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    crate::app::permissions_update(args, otto_core::approval::ApprovalSettings::default())?;
    command("Approval permissions", Command::Permissions(args.into()))
}

fn auto(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    crate::app::parse_auto_mode(args)?;
    command("Approval mode", Command::AutoMode(args.into()))
}
fn session(args: &str, info: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    push(Document {
        key: "session".into(),
        title: "Session".into(),
        text: format!(
            "{}\nID: {}\nWorkspace: {}\nProfile: {}\nModel: {}/{}\nReasoning: {}{}",
            info.session_name,
            info.session_id,
            info.workspace,
            info.profile,
            info.provider,
            info.model,
            info.thinking,
            info.forked_from
                .as_ref()
                .map(|origin| format!("\nForked from: {}", origin.session_id))
                .unwrap_or_default()
        ),
    })
}
fn plan(args: &str, info: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    push(Document {
        key: "plan".into(),
        title: "Plan".into(),
        text: if info.plan.is_empty() {
            "No plan. The agent keeps one with update_plan for work of three or more steps.".into()
        } else {
            format!(
                "{}\n\n{}",
                otto_core::session::plan_summary(&info.plan),
                otto_core::session::plan_checklist(&info.plan)
            )
        },
    })
}
fn resume(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args.is_empty() {
        push(Choice::new(ChoiceKind::Resume, ""))
    } else {
        command("Resume", Command::Resume(args.into()))
    }
}
fn fork(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args.is_empty() {
        push(Choice::new(ChoiceKind::Fork, ""))
    } else {
        command("Fork", Command::Fork(args.into()))
    }
}
fn archive(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args.is_empty() {
        push(Choice::new(ChoiceKind::Archive, ""))
    } else {
        command("Archive", Command::Archive(args.into()))
    }
}
fn model(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args.is_empty() {
        push(Choice::new(ChoiceKind::Profiles, ""))
    } else {
        command("Switch profile", Command::SwitchProfile(args.into()))
    }
}
fn thinking(args: &str, info: &Info, _: &Catalog) -> Result<Transition, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => push(Choice::new(
            ChoiceKind::Thinking { profile: None },
            &info.thinking,
        )),
        [level] if *level != "--save" => command(
            "Reasoning effort",
            Command::SetThinking {
                thinking: (*level).into(),
                save: false,
            },
        ),
        [level, "--save"] if *level != "--save" => command(
            "Reasoning effort",
            Command::SetThinking {
                thinking: (*level).into(),
                save: true,
            },
        ),
        _ => Err("usage: /thinking [LEVEL] [--save]".into()),
    }
}

fn rename(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args.is_empty() {
        push(Rename)
    } else {
        Ok(Transition::Request(Request::Command(Command::Rename(
            args.into(),
        ))))
    }
}
fn compact(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    Ok(Transition::Run(Run::Compact(args.into())))
}
fn reflect(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args == "list" || args.starts_with("show ") {
        management("reflect", args)
    } else {
        Ok(Transition::Run(Run::Reflect(args.into())))
    }
}
fn image(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args.is_empty() {
        Err("usage: /image <path>".into())
    } else {
        command("Attach image", Command::Image(args.into()))
    }
}
fn approve(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => push(Approvals::default()),
        [id] => command("Approve request", Command::Approve((*id).into())),
        [id, "always"] => command(
            "Approve persistent request",
            Command::ApproveAlways((*id).into()),
        ),
        _ => Err("usage: /approve [<id> [always]]".into()),
    }
}

/// `/steer <guidance>` is the explicit form of the text an active turn accepts.
/// Unlike ordinary text it never becomes a prompt: when nothing runs there is no
/// turn to guide, and silently starting one would hide the mistake.
fn steer(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args.is_empty() {
        return Err("usage: /steer <guidance>".into());
    }
    command("Guidance", Command::Steer(args.into()))
}

/// `/inputs` lists the active run's guidance, each line `#<id> <status> <text>`,
/// and `/inputs withdraw <id>` takes one back before it is delivered.
fn inputs(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => query("Guidance", Request::Command(Command::SteeringState)),
        ["withdraw", id] => command(
            "Withdraw guidance",
            Command::WithdrawSteer(
                id.parse()
                    .map_err(|_| "usage: /inputs withdraw <id>".to_string())?,
            ),
        ),
        _ => Err("usage: /inputs [withdraw <id>]".into()),
    }
}

/// How long one guidance line of `/inputs` may be, in characters.
const GUIDANCE_PREVIEW: usize = 60;

/// One guidance item per line: what the user can still withdraw, what was
/// already delivered, and what the run end dropped.
fn steering_lines(messages: &[otto_core::agent::steering::Message]) -> String {
    let mut text = if messages.is_empty() {
        "No guidance for the active task.".to_string()
    } else {
        messages
            .iter()
            .map(|message| {
                format!(
                    "#{} {} {}",
                    message.id,
                    steering_status(message.status),
                    guidance_preview(&message.text)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    text.push_str("\n\n/inputs withdraw <id> withdraws guidance that is still queued.");
    text
}

fn guidance_preview(text: &str) -> String {
    let single = crate::subagent::format::one_line(text);
    if single.chars().count() <= GUIDANCE_PREVIEW {
        return single;
    }
    format!(
        "{}…",
        single.chars().take(GUIDANCE_PREVIEW).collect::<String>()
    )
}

/// The status word one `/inputs` line uses. A queued item is the only one the
/// user can still withdraw; the others are what became of earlier guidance.
fn steering_status(status: otto_core::agent::steering::Status) -> &'static str {
    use otto_core::agent::steering::Status;
    match status {
        Status::Queued => "queued",
        Status::Delivered => "delivered",
        Status::Withdrawn => "withdrawn",
        Status::Discarded => "discarded",
    }
}
fn context(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    push(Context::new())
}
fn trace(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    push(super::trace::TraceView::new())
}
fn agents(args: &str, info: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    push(Agents::new(info.workspace.clone()))
}
fn task(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    management("task", args)
}

fn memory(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if args == "review" {
        push(Reviews::default())
    } else {
        management("memory", args)
    }
}
fn remember(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    management("remember", args)
}
fn skill(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    management("skill", args)
}
fn timers(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    management("timers", args)
}
fn mcp(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    if let Some(name) = args.strip_prefix("login ") {
        Ok(Transition::Run(Run::McpLogin(name.into())))
    } else {
        management("mcp", args)
    }
}
fn login(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    Ok(Transition::Run(Run::Login(args.into())))
}
fn logout(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    empty(args)?;
    command("Logout", Command::Logout)
}
fn sandbox(args: &str, info: &Info, _: &Catalog) -> Result<Transition, String> {
    let (sub, args) = args
        .split_once(char::is_whitespace)
        .map(|(s, a)| (s, a.trim()))
        .unwrap_or((args, ""));
    match (sub, args) {
        ("", _) => push(Document {
            key: "sandbox".into(),
            title: "Sandbox".into(),
            text: format!("{}\n{}", info.sandbox.summary(), info.sandbox.reason_code()),
        }),
        ("reload", "") => command("Reload sandbox", Command::SandboxReload),
        ("allow", path) if !path.is_empty() => {
            push(Choice::new(ChoiceKind::Allow(path.into()), ""))
        }
        ("network", "") => push(Choice::new(
            ChoiceKind::Network,
            match info.sandbox.network {
                crate::runtime::info::SandboxNetwork::Allowed => "allow",
                _ => "deny",
            },
        )),
        ("network", mode @ ("allow" | "deny")) => {
            command("Sandbox network", Command::SandboxNetwork(mode.into()))
        }
        ("exclude", entry) if !entry.is_empty() => command(
            "Sandbox exclusion",
            Command::SandboxExclude(crate::runtime::sandbox_config::parse_exclude_entry(entry)?),
        ),
        _ => {
            Err("usage: /sandbox [reload|allow <path>|network [allow|deny]|exclude <entry>]".into())
        }
    }
}

fn management(name: &str, args: &str) -> Result<Transition, String> {
    use crate::app::management::Request as M;
    let request = M::parse(name, args)?;
    let mutation = matches!(
        request,
        M::MemoryForget { .. }
            | M::MemoryReview { .. }
            | M::Remember { .. }
            | M::SkillSet { .. }
            | M::SkillRevert(_)
            | M::TimerCancel(_)
            | M::TaskCancel(_)
    );
    query(
        name,
        if mutation {
            Request::Command(Command::Management(request))
        } else {
            Request::Management(request)
        },
    )
}

struct RequestPresenter {
    retry: Option<Request>,
    title: String,
    request: Option<Request>,
    text: String,
    loaded: bool,
}
impl Presenter for RequestPresenter {
    fn key(&self) -> String {
        format!("command:{}", self.title)
    }
    fn view(&self) -> View {
        View {
            title: self.title.clone(),
            document: self.text.clone(),
            loading: !self.loaded,
            ..Default::default()
        }
    }
    fn start(&mut self) -> Option<Request> {
        self.request.take()
    }
    fn retry(&self) -> Option<Request> {
        self.retry.clone()
    }
    fn activate(&mut self, _: Activation) -> Transition {
        Transition::Stay
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        self.loaded = true;
        self.text = match reply {
            Reply::Image { path, .. } => format!("Attached image: {path}"),
            Reply::Operation { message, .. } => message,
            Reply::Steering(messages) => steering_lines(&messages),
            Reply::Management(reply) => crate::cli::repl_commands::format_management_reply(&reply),
            Reply::AgentDetail {
                row,
                history,
                missing,
            } => {
                let mut text = format!(
                    "{}\nStatus: {}\nResult: {}\nError: {}",
                    row.description, row.status, row.result, row.error
                );
                if missing {
                    text.push_str("\nTranscript unavailable");
                } else {
                    for entry in super::super::entries::entries_from_history(&history).0 {
                        text.push_str(&format!("\n{}\n{}", entry.raw, entry.tool_output));
                    }
                }
                text
            }
            _ => String::new(),
        };
        Transition::Stay
    }
}

struct Rename;
impl Presenter for Rename {
    fn key(&self) -> String {
        "rename".into()
    }
    fn view(&self) -> View {
        View {
            title: "Rename session".into(),
            fields: vec![Field {
                id: "name".into(),
                label: "Session name".into(),
                submit: true,
                max_bytes: 4096,
                ..Default::default()
            }],
            actions: vec![Action {
                id: "save".into(),
                label: "Rename".into(),
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    fn activate(&mut self, a: Activation) -> Transition {
        if let Activation::Action { fields, .. } = a {
            let name = fields.get("name").cloned().unwrap_or_default();
            return Transition::Request(Request::Command(Command::Rename(name)));
        }
        Transition::Stay
    }
    fn receive(&mut self, _: Reply) -> Transition {
        Transition::Close
    }
}
