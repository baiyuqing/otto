use std::collections::BTreeMap;
use std::sync::Arc;

use super::approval::Approval;
use super::choice::{Choice, ChoiceKind};
use super::collection::Agents;
use super::commands::Entry;
use super::review::Reviews;
use super::{Catalog, Document, Presenter, Transition};
use crate::app::Info;
use crate::memory::{Candidate, CandidateAction, CandidateState, Record, Scope};
use crate::tui::app::Backend;
use crate::tui::attach::Remote;
use crate::tui::backend::{Command, Reply, Request as BackendRequest};
use crate::tui::interaction::{Activation, Focus, Intent, State};
use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

fn approval_request() -> crate::approval::Request {
    crate::approval::Request {
        options: vec![],
        schema_version: 1,
        id: "approval-7".into(),
        revision: 7,
        kind: crate::approval::Kind::Authorization,
        scope: crate::approval::Scope {
            workspace: "/work".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        subject: crate::approval::Subject {
            tool_call_id: "call-1".into(),
            source: "tool".into(),
            id: "bash".into(),
            version: "1".into(),
            details: "run a command".into(),
        },
        title: "Run command".into(),
        reason: "requested by the user".into(),
        persistent: false,
        blocking: true,
        created_at: 1,
        expires_at: None,
        state: crate::approval::State::Pending,
        decision: None,
        application: crate::approval::ApplicationState::NotRequired,
        application_error: String::new(),
    }
}

fn candidate(id: &str) -> Candidate {
    Candidate {
        id: id.into(),
        proposed: Record {
            scope: Scope::new("workspace", "/work"),
            kind: "fact".into(),
            key: "color".into(),
            text: "favorite color is blue".into(),
            ..Record::default()
        },
        action: CandidateAction::Create,
        target_id: String::new(),
        base_revision: 0,
        reason: "stated directly".into(),
        state: CandidateState::Pending,
        created_at: DateTime::<Utc>::UNIX_EPOCH,
        decided_at: None,
        decision_source: None,
        result_record_id: String::new(),
        result_revision: 0,
    }
}

#[test]
fn approval_defaults_to_deny_without_dispatch_and_keeps_feedback_with_revision() {
    let request = approval_request();
    let mut approval = Approval::shared(&request);
    let mut state = State::new(&approval.view());

    assert_eq!(approval.view().default_action.as_deref(), Some("deny"));
    assert_eq!(state.selected_action.as_deref(), Some("deny"));
    state.focus = Focus::Field("feedback".into());
    state.handle(
        &approval.view(),
        Intent::Text("because it is needed".into()),
    );
    state.focus = Focus::Actions;
    state.pending = true;

    assert!(matches!(
        approval.receive(Reply::Operation {
            message: "application failed".into(),
            info: Info::default(),
            history: None,
            prompt: None,
        }),
        Transition::Stay
    ));
    state.pending = false;
    state.error = Some("application failed".into());
    state.reconcile(&approval.view());
    let mut refreshed = request.clone();
    refreshed.application = crate::approval::ApplicationState::Failed;
    refreshed.application_error = "application failed".into();
    approval.receive(Reply::ApprovalSnapshot(crate::approval::Snapshot {
        requests: vec![refreshed],
        cursor: 1,
    }));
    state.reconcile(&approval.view());
    assert_eq!(state.drafts["feedback"], "because it is needed");
    assert_eq!(state.error.as_deref(), Some("application failed"));

    state.focus = Focus::Field("feedback".into());
    state.handle(&approval.view(), Intent::NextFocus); // feedback -> actions
    assert_eq!(state.focus, Focus::Actions);
    state.handle(&approval.view(), Intent::Move(1)); // deny -> allow
    let activation = state.handle(&approval.view(), Intent::Activate).unwrap();
    assert!(matches!(
        approval.activate(activation),
        Transition::Request(BackendRequest::Command(Command::DecideApproval {
            id,
            allow: true,
            feedback,
            revision: Some(7),
        })) if id == "approval-7" && feedback == "because it is needed"
    ));
}

#[test]
fn memory_review_defaults_to_accept_but_only_dispatches_on_activation() {
    let mut reviews = Reviews::default();
    assert!(matches!(
        reviews.receive(Reply::Reviews(vec![candidate("candidate-2")])),
        Transition::Stay
    ));
    let view = reviews.view();
    assert_eq!(view.default_action.as_deref(), Some("accept"));
    assert!(matches!(
        reviews.activate(Activation::Open("candidate-2".into())),
        Transition::Stay
    ));

    let mut state = State::new(&view);
    assert_eq!(state.selected_action.as_deref(), Some("accept"));
    let activation = state.handle(&view, Intent::Activate).unwrap();
    assert!(matches!(
        reviews.activate(activation),
        Transition::Request(BackendRequest::Command(Command::MemoryReview { id, accept: true }))
            if id == "candidate-2"
    ));
}

#[test]
fn agent_collection_selection_tracks_the_row_id_after_refresh_reorders_rows() {
    let task = |parent: &str, id: &str| crate::subagent::record::TaskRow {
        parent_session: parent.into(),
        task_id: id.into(),
        ..Default::default()
    };
    let mut agents = Agents::new("/work".into());
    agents.receive(Reply::Agents(crate::subagent::record::ListResult {
        tasks: vec![task("parent", "one"), task("parent", "two")],
        ..Default::default()
    }));
    let first_view = agents.view();
    let mut state = State::new(&first_view);
    state.handle(&first_view, Intent::Move(1));
    let selected = state.selected_row.clone();

    agents.receive(Reply::Agents(crate::subagent::record::ListResult {
        tasks: vec![task("parent", "two"), task("parent", "one")],
        ..Default::default()
    }));
    state.reconcile(&agents.view());
    assert_eq!(state.selected_row, selected);
}

#[test]
fn thinking_apply_and_save_use_the_same_selected_effort() {
    let mut choice = Choice::new(ChoiceKind::Thinking { profile: None }, "medium");
    let apply = choice.activate(Activation::Action {
        id: "apply".into(),
        row: Some("xhigh".into()),
        fields: BTreeMap::new(),
    });
    let save = choice.activate(Activation::Action {
        id: "save".into(),
        row: Some("xhigh".into()),
        fields: BTreeMap::new(),
    });
    assert!(matches!(
        apply,
        Transition::Request(BackendRequest::Command(Command::SetThinking { thinking, save: false }))
            if thinking == "xhigh"
    ));
    assert!(matches!(
        save,
        Transition::Request(BackendRequest::Command(Command::SetThinking { thinking, save: true }))
            if thinking == "xhigh"
    ));
}

#[test]
fn fork_picker_lists_prompts_newest_first_and_forks_the_selected_one() {
    let mut choice = Choice::new(ChoiceKind::Fork, "");
    assert!(matches!(choice.refresh(), Some(BackendRequest::ForkPoints)));
    choice.receive(Reply::Sessions(crate::tui::backend::fork_point_choices(
        [
            ("00000001".to_string(), "first\nmore".to_string()),
            ("00000002".to_string(), "\nsecond".to_string()),
        ]
        .into_iter(),
    )));
    let view = choice.view();
    assert_eq!(
        view.rows
            .iter()
            .map(|row| (row.id.as_str(), row.cells[0].as_str()))
            .collect::<Vec<_>>(),
        [("00000002", "second"), ("00000001", "first")]
    );
    let fork = choice.activate(Activation::Open("00000001".into()));
    assert!(matches!(
        fork,
        Transition::Request(BackendRequest::Command(Command::Fork(id))) if id == "00000001"
    ));
}

#[test]
fn fork_picker_says_when_there_is_nothing_to_fork() {
    let mut choice = Choice::new(ChoiceKind::Fork, "");
    assert!(choice.view().document.is_empty());
    choice.receive(Reply::Sessions(Vec::new()));
    assert_eq!(
        choice.view().document,
        "This session has no prompt to fork before yet."
    );
}

fn custom_command(args: &str, _: &Info, _: &Catalog) -> Result<Transition, String> {
    Ok(Transition::Push(Box::new(Document {
        key: "custom".into(),
        title: "Custom presenter".into(),
        text: format!("custom {args}"),
    })))
}

#[test]
fn plan_command_and_line_show_the_session_plan_and_events_update_it() {
    use otto_core::session::{PlanStatus, PlanStep};
    let catalog = Catalog::default();
    let document = |info: &Info| {
        let Transition::Push(presenter) = catalog.open("/plan", info, false).unwrap() else {
            panic!("plan should open a document");
        };
        presenter.view().document
    };
    assert!(document(&Info::default()).starts_with("No plan."));
    assert_eq!(crate::tui::render::plan_line(&[]), None);

    let remote = Arc::new(Remote::offline(Vec::new()));
    let backend = Backend::Attach(&remote);
    let mut app = crate::tui::app::App::with_catalog(&backend, Catalog::default());
    app.apply_event(&otto_core::wire::events::to_wire(
        &otto_core::agent::Event::PlanUpdated {
            steps: vec![
                PlanStep {
                    text: "read".into(),
                    status: PlanStatus::Completed,
                },
                PlanStep {
                    text: "write".into(),
                    status: PlanStatus::InProgress,
                },
            ],
        },
    ));
    assert_eq!(
        crate::tui::render::plan_line(&app.info.plan).as_deref(),
        Some("Plan 1/2 · write")
    );
    assert_eq!(
        document(&app.info),
        "1 of 2 completed; in progress: write\n\n[x] read\n[>] write"
    );

    app.apply_event(&otto_core::wire::events::to_wire(
        &otto_core::agent::Event::PlanUpdated { steps: Vec::new() },
    ));
    assert_eq!(crate::tui::render::plan_line(&app.info.plan), None);
}

#[test]
fn registered_commands_extend_help_completion_and_dispatch() {
    let mut catalog = Catalog::default();
    catalog
        .register(Entry {
            name: "/extra",
            description: "run an extension command",
            local_only: false,
            factory: custom_command,
        })
        .unwrap();

    let help = catalog.open("/help", &Info::default(), false).unwrap();
    let Transition::Push(presenter) = help else {
        panic!("help should open a document");
    };
    assert!(
        presenter
            .view()
            .document
            .contains("/extra  run an extension command")
    );

    let remote = Arc::new(Remote::offline(Vec::new()));
    let backend = Backend::Attach(&remote);
    let mut app = crate::tui::app::App::with_catalog(&backend, catalog);
    app.insert_text("/ext");
    assert!(
        app.suggestions()
            .iter()
            .any(|item| item.replacement == "/extra")
    );
    assert!(
        app.dispatch_line("/extra value", &backend, &CancellationToken::new())
            .is_none()
    );
    assert_eq!(
        app.surfaces.top().unwrap().model.view().document,
        "custom value"
    );
}
