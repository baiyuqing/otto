use std::collections::BTreeMap;
use std::sync::Arc;

use super::approval::{Approval, Approvals};
use super::choice::{Choice, ChoiceKind};
use super::collection::Agents;
use super::commands::{CommandGroup, Entry};
use super::review::Reviews;
use super::{Catalog, Document, Presenter, Run, Transition};
use crate::app::Info;
use crate::memory::{Candidate, CandidateAction, CandidateState, Record, Scope};
use crate::tui::app::{Action, Backend};
use crate::tui::attach::Remote;
use crate::tui::backend::{Command, Reply, Request as BackendRequest};
use crate::tui::interaction::{Activation, Focus, Intent, State};
use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

#[test]
fn skill_manager_command_is_available_locally_and_attached_and_keeps_text_forms() {
    use crate::app::management::Request as M;
    let catalog = Catalog::default();
    for attached in [false, true] {
        let Transition::Push(mut presenter) =
            catalog.open("/skill", &Info::default(), attached).unwrap()
        else {
            panic!("manager surface expected")
        };
        assert_eq!(presenter.key(), "skills");
        assert!(matches!(
            presenter.start(),
            Some(BackendRequest::Management(M::SkillManager))
        ));
        let Transition::Push(mut detail) = catalog
            .open("/skill lint", &Info::default(), attached)
            .unwrap()
        else {
            panic!("text detail expected")
        };
        assert!(
            matches!(detail.start(), Some(BackendRequest::Management(M::SkillDetail(name))) if name == "lint")
        );
        let Transition::Push(mut set) = catalog
            .open("/skill set lint enabled", &Info::default(), attached)
            .unwrap()
        else {
            panic!("text mutation expected")
        };
        assert!(matches!(
            set.start(),
            Some(BackendRequest::Command(Command::Management(M::SkillSet {
                enabled: true,
                ..
            })))
        ));
    }
}

fn approval_request() -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: vec![],
        schema_version: 1,
        id: "approval-7".into(),
        revision: 7,
        kind: otto_core::approval::Kind::Authorization,
        authority: otto_core::approval::Authority::Boundary,
        scope: otto_core::approval::Scope {
            workspace: "/work".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
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
        state: otto_core::approval::State::Pending,
        decision: None,
        application: otto_core::approval::ApplicationState::NotRequired,
        application_error: String::new(),
        review: None,
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
fn approval_activity_inspection_separates_counts_and_review_has_no_actions() {
    let mut request = approval_request();
    request.review = Some(otto_core::approval::ReviewMetadata {
        status: otto_core::approval::ReviewStatus::Reviewing,
        assessment: None,
        settings_revision: 1,
        evidence_digest: "evidence".into(),
        policy_digest: "policy".into(),
    });
    let id = request.id.clone();
    let mut approvals = Approvals::default();
    approvals.receive(Reply::ApprovalActivity(crate::approval::ApprovalActivity {
        requests: Vec::new(),
        cursor: 1,
        reviewing: vec![request],
        recent_decisions: Vec::new(),
        counts: crate::approval::ApprovalCounts {
            human_pending: 0,
            reviewing: 1,
            application_failed: 2,
        },
    }));
    assert_eq!(
        approvals.view().description,
        "0 human pending · 1 reviewing · 2 application failed"
    );
    let Transition::Push(detail) = approvals.activate(Activation::Open(id)) else {
        panic!("open review details");
    };
    assert!(detail.view().actions.is_empty());
}

#[test]
fn automatic_review_and_denial_have_no_human_actions() {
    let mut request = approval_request();
    request.review = Some(otto_core::approval::ReviewMetadata {
        status: otto_core::approval::ReviewStatus::Reviewing,
        assessment: None,
        settings_revision: 1,
        evidence_digest: "evidence".into(),
        policy_digest: "policy".into(),
    });
    let view = Approval::shared(&request).view();
    assert!(view.actions.is_empty());
    assert!(view.default_action.is_none());
    assert_eq!(view.description.split(" · ").next(), Some("Reviewing"));
    request.state = otto_core::approval::State::Rejected;
    let review = request.review.as_mut().unwrap();
    review.status = otto_core::approval::ReviewStatus::Completed;
    review.assessment = Some(otto_core::approval::ReviewAssessment {
        risk: otto_core::approval::RiskLevel::Critical,
        user_authorization: otto_core::approval::UserAuthorization::Unknown,
        outcome: otto_core::approval::ReviewOutcome::Deny,
        reason: "The target is outside the authorized scope.".into(),
    });
    let view = Approval::shared(&request).view();
    assert!(view.actions.is_empty());
    assert!(view.document.contains("outside the authorized scope"));
}

#[test]
fn approval_defaults_to_approve_without_feedback() {
    let request = approval_request();
    let mut approval = Approval::shared(&request);
    let view = approval.view();
    let mut state = State::new(&view);

    assert!(view.fields.is_empty());
    assert_eq!(view.default_action.as_deref(), Some("allow"));
    assert_eq!(state.selected_action.as_deref(), Some("allow"));
    assert_eq!(state.focus, Focus::Actions);

    let activation = state.handle(&view, Intent::Activate).unwrap();
    assert!(matches!(
        approval.activate(activation),
        Transition::Request(BackendRequest::Command(Command::DecideApproval {
            id,
            allow: true,
            feedback,
            revision: Some(7),
        })) if id == "approval-7" && feedback.is_empty()
    ));

    assert!(matches!(
        approval.receive(Reply::Operation {
            message: "Approved approval-7.".into(),
            info: Info::default(),
            history: None,
            prompt: None,
        }),
        Transition::Close
    ));
}

#[test]
fn approval_refresh_keeps_the_exact_revision_and_approve_default() {
    let request = approval_request();
    let mut approval = Approval::shared(&request);
    let mut refreshed = request.clone();
    refreshed.revision = 8;
    approval.receive(Reply::ApprovalSnapshot(otto_core::approval::Snapshot {
        requests: vec![refreshed],
        cursor: 1,
    }));
    let view = approval.view();
    let mut state = State::new(&view);
    let activation = state.handle(&view, Intent::Activate).unwrap();
    assert!(matches!(
        approval.activate(activation),
        Transition::Request(BackendRequest::Command(Command::DecideApproval {
            revision: Some(8),
            allow: true,
            feedback,
            ..
        })) if feedback.is_empty()
    ));
}

#[test]
fn empty_memory_review_has_an_explicit_empty_state() {
    let mut reviews = Reviews::default();
    assert!(matches!(
        reviews.receive(Reply::Reviews(Vec::new())),
        Transition::Stay
    ));

    let view = reviews.view();
    assert_eq!(view.document, "No pending memory candidates.");
    assert!(!view.loading);
    assert!(view.rows.is_empty());
    assert!(view.actions.iter().all(|action| !action.enabled));
}

#[test]
fn memory_review_exposes_full_candidate_information_as_selected_row_detail() {
    let mut reviews = Reviews::default();
    reviews.receive(Reply::Reviews(vec![candidate("candidate-2")]));

    let view = reviews.view();
    let detail = &view.rows[0].detail;
    assert!(detail.contains("favorite color is blue"));
    assert!(detail.contains("Reason: stated directly"));
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
fn sandbox_exclude_runs_inline_but_read_grants_still_require_confirmation() {
    let catalog = Catalog::default();
    for entry in ["gh *", "'gh *'", "\"gh *\""] {
        assert!(matches!(
            catalog.open(&format!("/sandbox exclude {entry}"), &Info::default(), false).unwrap(),
            Transition::Request(BackendRequest::Command(Command::SandboxExclude(value)))
                if value == "gh *"
        ));
    }
    assert!(matches!(
        catalog
            .open("/sandbox allow /tmp", &Info::default(), false)
            .unwrap(),
        Transition::Push(_)
    ));
    assert!(
        catalog
            .open("/sandbox exclude ''", &Info::default(), false)
            .is_err()
    );
    assert!(
        catalog
            .open("/sandbox exclude 'gh *'", &Info::default(), true)
            .err()
            .unwrap()
            .contains("not available with --attach")
    );
}

#[test]
fn clear_starts_a_new_session_without_opening_a_command_panel() {
    let catalog = Catalog::default();
    assert!(matches!(
        catalog.open("/clear", &Info::default(), false).unwrap(),
        Transition::Request(BackendRequest::Command(Command::NewSession))
    ));
    assert!(matches!(
        catalog.open("/new", &Info::default(), false).unwrap(),
        Transition::Push(_)
    ));
}

#[test]
fn auto_is_a_hidden_alias_of_permissions() {
    let catalog = Catalog::default();
    assert!(catalog.entries().iter().all(|entry| entry.name != "/auto"));
    assert!(
        catalog
            .entries()
            .iter()
            .any(|entry| entry.name == "/permissions")
    );
    for line in ["/auto yolo", "/permissions yolo"] {
        match catalog.open(line, &Info::default(), false).unwrap() {
            Transition::Request(BackendRequest::Command(Command::Permissions(args))) => {
                assert_eq!(args, "yolo")
            }
            Transition::Push(_) => {}
            _ => panic!("{line}"),
        }
    }
    assert!(
        catalog
            .open("/auto bogus", &Info::default(), false)
            .is_err()
    );
}

#[test]
fn rename_with_a_name_runs_without_opening_a_result_panel() {
    let catalog = Catalog::default();
    assert!(matches!(
        catalog
            .open("/rename release notes", &Info::default(), false)
            .unwrap(),
        Transition::Request(BackendRequest::Command(Command::Rename(name)))
            if name == "release notes"
    ));
}

#[test]
fn rename_without_a_name_still_opens_the_form() {
    let catalog = Catalog::default();
    let Transition::Push(presenter) = catalog.open("/rename", &Info::default(), false).unwrap()
    else {
        panic!("bare /rename should open the form");
    };

    let view = presenter.view();
    assert_eq!(view.title, "Rename session");
    assert_eq!(view.fields[0].label, "Session name");
}

#[test]
fn thinking_apply_and_save_use_the_same_selected_effort() {
    let mut choice = Choice::new(ChoiceKind::Thinking { model: None }, "medium");
    assert!(matches!(
        choice.receive(Reply::Operation {
            message: "Thinking: medium".into(),
            info: Info::default(),
            history: None,
            prompt: None,
        }),
        Transition::CloseFlow
    ));

    let mut choice = Choice::new(ChoiceKind::Thinking { model: None }, "medium");

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
fn inputs_is_not_recognized_or_listed_locally_or_attached() {
    let catalog = Catalog::default();
    assert!(
        catalog
            .entries()
            .iter()
            .all(|entry| entry.name != "/inputs")
    );
    for attached in [false, true] {
        for line in ["/inputs", "/inputs withdraw 12"] {
            assert_eq!(
                catalog.open(line, &Info::default(), attached).err(),
                Some(format!("unknown command: {line}"))
            );
        }
        let Transition::Push(help) = catalog.open("/help", &Info::default(), attached).unwrap()
        else {
            panic!("help should open a document");
        };
        assert!(!help.view().document.contains("/inputs"));
    }

    let remote = Arc::new(Remote::offline(Vec::new()));
    let backend = Backend::Attach(&remote);
    let mut app = crate::tui::app::App::with_catalog(&backend, catalog);
    app.insert_text("/in");
    assert!(
        app.suggestions()
            .iter()
            .all(|item| item.replacement != "/inputs")
    );
    assert!(
        app.suggestions()
            .iter()
            .any(|item| item.replacement == "/init")
    );
}

#[test]
fn the_steer_slash_command_is_unknown() {
    let catalog = Catalog::default();
    for line in ["/steer use the other branch", "/steer"] {
        assert_eq!(
            catalog.open(line, &Info::default(), false).err(),
            Some(format!("unknown command: {line}"))
        );
    }
}

#[test]
fn plan_line_shows_the_session_plan_and_events_update_it() {
    use otto_core::session::{PlanStatus, PlanStep};
    let catalog = Catalog::default();
    assert!(catalog.open("/plan", &Info::default(), false).is_err());
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

    app.apply_event(&otto_core::wire::events::to_wire(
        &otto_core::agent::Event::PlanUpdated { steps: Vec::new() },
    ));
    assert_eq!(crate::tui::render::plan_line(&app.info.plan), None);
}

#[test]
fn tasks_alias_dispatches_without_appearing_in_help() {
    let catalog = Catalog::default();
    let help = catalog.open("/help", &Info::default(), false).unwrap();
    let Transition::Push(presenter) = help else {
        panic!("help should open a document");
    };
    let help = presenter.view().document;
    assert!(
        help.contains("/task  list, inspect or cancel tasks"),
        "{help}"
    );
    assert!(!help.contains("/tasks"), "{help}");

    let Ok(Transition::Push(canonical)) = catalog.open("/task", &Info::default(), false) else {
        panic!("/task should open the task-list presenter");
    };
    let Ok(Transition::Push(alias)) = catalog.open("/tasks", &Info::default(), false) else {
        panic!("/tasks should remain a task-list alias");
    };
    assert_eq!(canonical.view().title, alias.view().title);
}

#[test]
fn quit_alias_exits_without_appearing_in_help() {
    let catalog = Catalog::default();
    let help = catalog.open("/help", &Info::default(), false).unwrap();
    let Transition::Push(presenter) = help else {
        panic!("help should open a document");
    };
    let help = presenter.view().document;
    assert!(help.contains("/exit  exit Otto"), "{help}");
    assert!(!help.contains("/quit"), "{help}");

    assert!(matches!(
        catalog.open("/exit", &Info::default(), false),
        Ok(Transition::Run(Run::Exit))
    ));
    assert!(matches!(
        catalog.open("/quit", &Info::default(), false),
        Ok(Transition::Run(Run::Exit))
    ));
    assert_eq!(
        catalog.open("/quit now", &Info::default(), false).err(),
        Some("unexpected arguments".into())
    );
}

#[test]
fn removed_attachment_and_trace_commands_are_absent_from_help_completion_and_dispatch() {
    let catalog = Catalog::default();
    for attached in [false, true] {
        for line in [
            "/image",
            "/image /tmp/picture with spaces.png",
            "/trace",
            "/trace extra",
        ] {
            assert!(matches!(
                catalog.open(line, &Info::default(), attached),
                Err(error) if error == format!("unknown command: {line}")
            ));
        }
        let Transition::Push(help) = catalog.open("/help", &Info::default(), attached).unwrap()
        else {
            panic!("help should open a document");
        };
        assert!(!help.view().document.contains("/image"));
        assert!(!help.view().document.contains("/trace"));
    }
    assert!(
        !catalog
            .entries()
            .iter()
            .any(|entry| matches!(entry.name, "/image" | "/trace"))
    );

    let remote = Arc::new(Remote::offline(Vec::new()));
    let backend = Backend::Attach(&remote);
    for attached in [false, true] {
        let mut app = crate::tui::app::App::with_catalog(&backend, Catalog::default());
        app.attached = attached;
        for prefix in ["/ima", "/tra"] {
            app.insert_text(prefix);
            assert!(app.suggestions().is_empty());
            app.composer.take();
        }
        assert!(
            app.dispatch_line("/trace", &backend, &CancellationToken::new())
                .is_none()
        );
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.raw.contains("unknown command: /trace"))
        );
        assert!(
            app.dispatch_line("/image picture.png", &backend, &CancellationToken::new())
                .is_none()
        );
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.raw.contains("unknown command: /image picture.png"))
        );
        assert!(matches!(
            app.dispatch_line("describe this image", &backend, &CancellationToken::new()),
            Some(Action::Prompt(line)) if line == "describe this image"
        ));
    }
}

#[test]
fn registered_commands_extend_help_completion_and_dispatch() {
    let mut catalog = Catalog::default();
    catalog
        .register(Entry {
            name: "/extra",
            group: CommandGroup::Exit,
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

#[test]
fn memory_restore_is_a_mutation_and_history_and_diff_are_queries() {
    use crate::app::management::Request as M;
    let catalog = Catalog::default();
    let start = |command: &str| {
        let Transition::Push(mut presenter) =
            catalog.open(command, &Info::default(), false).unwrap()
        else {
            panic!("{command} should open a surface")
        };
        presenter.start().expect("a backend request")
    };
    assert!(matches!(
        start("/memory restore r1 2"),
        BackendRequest::Command(Command::Management(M::MemoryRestore { revision: 2, .. }))
    ));
    assert!(matches!(
        start("/memory history r1"),
        BackendRequest::Management(M::MemoryHistory { .. })
    ));
    assert!(matches!(
        start("/memory diff r1 1 2"),
        BackendRequest::Management(M::MemoryDiff {
            from: 1,
            to: Some(2),
            ..
        })
    ));
}
