
use crossterm::event::KeyCode;
use otto_core::agent::Event;
use otto_core::model::Usage;
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};

use super::*;
use crate::cli::testutil;
use crate::tui::entries::{Entry, EntryKind};
use crate::tui::wire_frame;

async fn app_fixture() -> (tempfile::TempDir, tempfile::TempDir, App) {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = testutil::controller(workspace.path(), sessions.path()).await;
    let app = App::new(&crate::tui::app::Backend::Local(&controller));
    (workspace, sessions, app)
}

fn rendered(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, app)).expect("draw");
    format!("{}", terminal.backend())
}

fn approval() -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: Vec::new(),
        schema_version: 1,
        id: "approval-1".into(),
        revision: 1,
        kind: otto_core::approval::Kind::Confirmation,
        authority: otto_core::approval::Authority::Consent,
        scope: otto_core::approval::Scope {
            workspace: "/work".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
            tool_call_id: String::new(),
            source: "conversation".into(),
            id: "plan".into(),
            version: "1".into(),
            details: "Review the implementation plan".into(),
        },
        title: "Approve implementation".into(),
        reason: "Before editing".into(),
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

#[tokio::test]
async fn every_feature_draws_as_a_centered_panel_over_the_session_shell() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries.push(Entry {
        kind: Some(EntryKind::User),
        raw: "underlying conversation".into(),
        ..Entry::default()
    });
    let remote = std::sync::Arc::new(crate::tui::attach::Remote::offline(Vec::new()));
    let backend = super::super::app::Backend::Attach(&remote);
    app.dispatch_line(
        "/help",
        &backend,
        &tokio_util::sync::CancellationToken::new(),
    );

    let screen = rendered(&app, 100, 24);
    assert!(screen.contains("underlying conversation"), "{screen}");
    assert!(screen.contains("Commands"), "{screen}");
    assert!(screen.contains("/help  show commands"), "{screen}");
    assert!(screen.contains("AUTO | tokens"), "{screen}");
    let rows = screen_rows(&app, 100, 24);
    assert!(
        rows.iter()
            .any(|row| row.contains('╭') && row.contains('╮'))
    );
    assert!(
        !rows[0].contains('╭'),
        "panel reached the top edge: {rows:?}"
    );
}

#[tokio::test]
async fn approval_draws_as_a_centered_panel_over_the_session_shell() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries.push(Entry {
        kind: Some(EntryKind::User),
        raw: "underlying conversation".into(),
        ..Entry::default()
    });
    app.sync_shared_approval(&[approval()]);

    let screen = rendered(&app, 100, 24);
    assert!(screen.contains("underlying conversation"), "{screen}");
    assert!(screen.contains("Approve implementation"), "{screen}");
    assert!(screen.contains("[y] Approve"), "{screen}");
    assert!(screen.contains("AUTO | tokens"), "{screen}");
    let rows = screen_rows(&app, 100, 24);
    assert!(
        rows.iter()
            .any(|row| row.contains('╭') && row.contains('╮'))
    );
    assert!(
        !rows[0].contains('╭'),
        "panel reached the top edge: {rows:?}"
    );
}

/// The drawn rows as plain text, without `TestBackend`'s `Display`
/// quoting and without the layout's side margin, so a test can assert on
/// the gutter a reader actually sees.
fn screen_rows(app: &App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            let row: String = (0..width)
                .filter_map(|x| buffer.cell((x, y)))
                .map(|cell| cell.symbol())
                .collect();
            row.trim_end()
                .chars()
                .skip(SIDE_MARGIN as usize)
                .collect::<String>()
        })
        .collect()
}

#[test]
fn skill_manager_renders_selection_preview_and_scrolling_in_small_terminals() {
    use crate::tui::features::Presenter;
    use crate::tui::interaction::Intent;
    let remote = crate::tui::attach::Remote::offline(Vec::new());
    let mut app = App::new(&crate::tui::app::Backend::Attach(&remote));
    let mut skills = crate::tui::features::skills::Skills::default();
    let rows = (0..80)
        .map(|i| crate::app::management::ManagedSkill {
            skill: crate::skill::Skill {
                name: format!("skill-{i:02}"),
                description: format!("Preview description {i}"),
                contract: None,
                directory: "/skills/demo".into(),
                path: "/skills/demo/SKILL.md".into(),
            },
            enabled: false,
            loaded: true,
        })
        .collect();
    skills.receive(crate::tui::backend::Reply::Management(
        crate::app::management::Reply::SkillManager(crate::app::management::SkillManager {
            global_enabled: true,
            skills: rows,
        }),
    ));
    let view = skills.view();
    app.surfaces.push(Box::new(skills), &view);
    let wide = screen_rows(&app, 120, 35).join("\n");
    assert!(wide.contains("Preview description 0"), "{wide}");
    assert!(wide.contains("[disabled]"), "{wide}");
    assert!(wide.contains("Per-skill preference: disabled"), "{wide}");
    app.surfaces
        .top_mut()
        .unwrap()
        .state
        .handle(&view, Intent::Move(79));
    let scrolled = screen_rows(&app, 120, 35).join("\n");
    assert!(scrolled.contains("skill-79"), "{scrolled}");
    for (width, height) in [(40, 8), (55, 15), (10, 3)] {
        let rows = screen_rows(&app, width, height);
        assert_eq!(rows.len(), height as usize);
    }
}

fn trace_app() -> App {
    let remote = crate::tui::attach::Remote::offline(Vec::new());
    let mut app = App::new(&crate::tui::app::Backend::Attach(&remote));
    let mut trace = crate::tui::features::TraceView::new();
    let _ = crate::tui::features::Presenter::receive(
        &mut trace,
        crate::tui::backend::Reply::Trace(crate::tui::features::sample_trace()),
    );
    let view = crate::tui::features::Presenter::view(&trace);
    app.surfaces.push(Box::new(trace), &view);
    app
}

const BAR_GLYPHS: [char; 4] = ['░', '▒', '◇', '█'];

#[test]
fn trace_bars_are_drawn_only_when_the_panel_is_wide_enough() {
    let app = trace_app();
    let wide = screen_rows(&app, 100, 30).join("\n");
    assert!(wide.contains("model #1"), "{wide}");
    assert!(wide.contains('░') && wide.contains('█'), "{wide}");
    assert!(wide.contains("10s"), "axis marks: {wide}");

    let narrow = screen_rows(&app, 79, 30).join("\n");
    assert!(narrow.contains("model #1"), "{narrow}");
    assert!(!narrow.contains(BAR_GLYPHS), "{narrow}");
}

/// The drawn rows above the status line, joined.
///
/// The status line is the one row whose content a test cannot predict: it
/// shows the workspace's last path component and the session id, which a
/// temporary-directory fixture draws at random. A two-character task id
/// like `t9` therefore occurs there by chance, so an "this id does not
/// appear" assertion may only scan the rows above it. The panel is drawn
/// within that range, so the assertion still catches a panel row.
fn above_status_line(rows: &[String]) -> String {
    rows[..rows.len().saturating_sub(1)].join("\n")
}

/// The model id is the longest field of the footer's fixed prefix, so the
/// token and cache totals used to sit past the right edge of any terminal
/// that a long id like `deepseek-v4-flash` did not leave room on, where
/// ratatui clips them.
#[tokio::test]
async fn a_long_model_id_keeps_the_token_and_cache_totals_visible() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.info.model = "deepseek-v4-flash".to_string();
    app.usage = Usage {
        input_tokens: 93_498 + 7_091_584,
        output_tokens: 37_475,
        cached_input_tokens: 7_091_584,
    };
    app.cache_breakdown_complete = Some(true);

    for width in [72u16, 80, 88, 100] {
        let rows = screen_rows(&app, width, MIN_TERMINAL_HEIGHT);
        let footer = rows.last().expect("footer row");
        assert!(
            footer.contains("tokens 7.2M/37.5k") && footer.contains("(cached 7.1M, 98.7%)"),
            "{width} columns: {footer}"
        );
    }
}

#[tokio::test]
async fn footer_omits_the_profile_and_uses_compact_status_labels() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.info.profile = "terra".to_string();
    app.info.model = "gpt-5.6-terra".to_string();
    app.info.thinking.clear();
    app.info.sandbox = testutil::seatbelt_info(crate::runtime::info::SandboxNetwork::Allowed);

    let footer = footer_text(&app);

    assert!(
        footer.starts_with("gpt-5.6-terra | AUTO | tokens 0/0 | "),
        "{footer}"
    );
    assert!(
        footer.contains(" | sb · write · net | default | "),
        "{footer}"
    );
    assert!(!footer.contains("terra/gpt-5.6-terra"), "{footer}");
    assert!(!footer.contains("think default"), "{footer}");
    assert!(!footer.contains("workspace-write"), "{footer}");
    assert!(!footer.contains("network allowed"), "{footer}");
}

#[tokio::test]
async fn empty_startup_transcript_shows_the_otto_logo() {
    let (_workspace, _sessions, app) = app_fixture().await;

    let screen = screen_rows(&app, 80, 20).join("\n");

    assert!(screen.contains("____  __  __"), "{screen}");
    assert!(screen.contains("/ __ \\/ /_/ /____"), "{screen}");
    assert!(screen.contains("/ /_/ / __/ __/ __ \\"), "{screen}");
    assert!(screen.contains("\\____/\\__/\\__/\\____/"), "{screen}");
}

/// The drawn frame's buffer, for the tests that assert on colour rather
/// than on glyphs.
fn drawn(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, app)).expect("draw");
    terminal.backend().buffer().clone()
}

/// The first row whose leftmost transcript cell carries the prompt band.
fn banded_row(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Option<u16> {
    (0..height).find(|y| {
        (SIDE_MARGIN..width - SIDE_MARGIN).any(|x| {
            buffer
                .cell((x, *y))
                .is_some_and(|cell| cell.bg == transcript::PROMPT_BACKGROUND)
        })
    })
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// The spinner advances, but model phases never expose elapsed duration.
#[test]
fn the_status_line_animates_without_thinking_or_turn_duration() {
    let at = |_phase_ms, turn_ms| {
        line_text(&thinking_line(&TurnStatus {
            phase: "reasoning".into(),
            turn_elapsed: Duration::from_millis(turn_ms),
        }))
    };

    assert_eq!(at(0, 0), "⠋ reasoning");
    assert_eq!(at(0, 100), "⠙ reasoning");
    assert_eq!(at(1_000, 5_000), "⠋ reasoning");
}

/// A turn streams nothing until the model's first token, so without this
/// line the transcript sits unchanged and the terminal looks hung.
#[tokio::test]
async fn a_running_turn_shows_the_thinking_line_under_the_transcript() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries.push(Entry {
        kind: Some(EntryKind::User),
        raw: "hello otto".to_string(),
        ..Default::default()
    });

    let idle = rendered(&app, 50, 10);
    app.start_turn();
    let busy = rendered(&app, 50, 10);
    app.apply_event(&wire_frame(&Event::ToolCallStarted {
        operation_id: otto_core::model::OperationId::new("op_test").expect("operation id"),
        attempt: 1,
        tool_name: "bash".into(),
        tool_call_id: "c1".into(),
        arguments: r#"{"command":"ls"}"#.into(),
    }));
    let running = rendered(&app, 50, 10);
    app.end_turn();

    assert!(!idle.contains("waiting"), "idle transcript:\n{idle}");
    assert!(
        busy.contains("waiting for model"),
        "running transcript:\n{busy}"
    );
    assert!(
        running.contains("running bash") && !running.contains(r#"{"command":"ls"}"#),
        "tool transcript:\n{running}"
    );
    assert!(!rendered(&app, 50, 10).contains("turn"));
}

#[tokio::test]
async fn the_busy_composer_title_says_which_turn_enter_queues_for() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.start_turn();
    assert!(rendered(&app, 80, 12).contains("Enter queues for this turn"));

    app.attached = true;
    assert!(rendered(&app, 80, 12).contains("Enter queues the next turn"));
}

#[tokio::test]
async fn model_feedback_is_pinned_in_a_banded_row_above_the_composer() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = (0..12)
        .map(|index| Entry {
            kind: Some(EntryKind::Assistant),
            raw: format!("old transcript row {index}"),
            ..Default::default()
        })
        .collect();
    app.scroll = Some(0);
    app.composer_notices = vec!["Thinking: high. Saved to profile.".into()];

    let width = 72;
    let height = 12;
    let rows = screen_rows(&app, width, height);
    let banner_row = rows
        .iter()
        .position(|row| row.contains("◆ UPDATED  Thinking: high. Saved to profile."))
        .expect("model feedback row");
    assert!(rows[banner_row + 1].contains("Otto"), "{rows:?}");
    assert!(
        !app.entries
            .iter()
            .any(|entry| entry.raw.contains("Thinking: high")),
        "model feedback must not be a transcript entry"
    );

    let buffer = drawn(&app, width, height);
    for x in SIDE_MARGIN..width - SIDE_MARGIN {
        assert_eq!(
            buffer.cell((x, banner_row as u16)).map(|cell| cell.bg),
            Some(QUEUED_BACKGROUND),
            "model feedback must shade the full content row at x={x}"
        );
    }
}

#[tokio::test]
async fn multiline_model_feedback_uses_one_banded_row_per_message() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.composer_notices = vec![
        "Switched to profile terra (provider chatgpt, model gpt-terra). Set as default profile."
            .into(),
        "Thinking: medium".into(),
    ];

    let rows = screen_rows(&app, 100, 12);
    let profile_row = rows
        .iter()
        .position(|row| row.contains("Set as default profile."))
        .expect("profile feedback row");
    assert!(rows[profile_row + 1].contains("◆ UPDATED  Thinking: medium"));
    assert!(rows.iter().all(|row| !row.contains(r"\x0a")), "{rows:?}");
}

#[tokio::test]
async fn queued_input_is_pinned_in_a_banded_row_above_the_composer() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = (0..12)
        .map(|index| Entry {
            kind: Some(EntryKind::Assistant),
            raw: format!("old transcript row {index}"),
            ..Default::default()
        })
        .collect();
    app.scroll = Some(0);
    app.start_turn();
    app.queued_input = Some("/memory search vim".to_string());
    app.apply_event(&otto_core::wire::events::WireEvent {
        event_type: otto_core::wire::events::USER_MESSAGE.into(),
        text: "guidance for current turn".into(),
        ..Default::default()
    });

    let width = 72;
    let height = 12;
    let rows = screen_rows(&app, width, height);
    let banner_row = rows
        .iter()
        .position(|row| row.contains("◆ QUEUED NEXT INPUT  /memory search vim"))
        .expect("queued banner row");
    assert!(rows[banner_row + 1].contains("Queued next input · Ctrl+U withdraw"));
    assert!(
        !rows.iter().any(|row| row.contains("❯ Queued next input")),
        "queued input must not be a transcript entry: {rows:?}"
    );

    let buffer = drawn(&app, width, height);
    for x in SIDE_MARGIN..width - SIDE_MARGIN {
        assert_eq!(
            buffer.cell((x, banner_row as u16)).map(|cell| cell.bg),
            Some(QUEUED_BACKGROUND),
            "queued banner must shade the full content row at x={x}"
        );
    }
}

/// Guidance the active turn has accepted but not delivered is pinned above
/// the composer, so the frame says both that it waits and how to take it back.
#[tokio::test]
async fn queued_guidance_draws_in_the_banner_and_the_composer_title() {
    use crate::tui::backend::{Completion, Reply, Ticket};
    use otto_core::agent::steering::{Message, Status};

    let (_workspace, _sessions, mut app) = app_fixture().await;
    let message = |status| Message {
        id: 3,
        text: "use the other branch".to_string(),
        status,
        input_id: "steer:turn:1".to_string(),
    };
    // The run's own read is what paints the queued guidance; the frame test
    // goes through it rather than setting the field directly.
    let (workspace, session) = (app.info.workspace.clone(), app.info.session_id.clone());
    let read = |messages| Completion {
        mutation: true,
        ticket: Ticket {
            workspace: workspace.clone(),
            session: session.clone(),
            surface: 0,
            generation: 1,
        },
        result: Ok(Reply::Steering(messages)),
    };
    app.start_turn();
    app.apply_completion(read(vec![message(Status::Queued)]));

    let screen = rendered(&app, 72, 12);
    assert!(
        screen.contains("◆ QUEUED FOR CURRENT TURN  use the other branch"),
        "{screen}"
    );
    assert!(
        screen.contains("Queued for current turn · Ctrl+U recall to edit · Esc cancels turn"),
        "{screen}"
    );
    assert!(
        !screen.contains("│use the other branch"),
        "queued guidance should not remain in the composer:\n{screen}"
    );

    // A withdrawal, then a delivery, each leave nothing to draw.
    app.apply_completion(read(vec![message(Status::Withdrawn)]));
    let screen = rendered(&app, 72, 12);
    assert!(!screen.contains("Queued for current turn"), "{screen}");
    assert!(!screen.contains("Ctrl+U withdraw"), "{screen}");

    app.apply_completion(read(vec![message(Status::Queued)]));
    assert!(rendered(&app, 72, 12).contains("◆ QUEUED FOR CURRENT TURN  use the other branch"));
    app.apply_event(&otto_core::wire::events::WireEvent {
        event_type: otto_core::wire::events::USER_MESSAGE.into(),
        text: "use the other branch".into(),
        ..Default::default()
    });
    let screen = rendered(&app, 72, 12);
    assert!(!screen.contains("Queued for current turn"), "{screen}");
    assert!(!screen.contains("Ctrl+U withdraw"), "{screen}");
}

/// The drawn frame, not the row builder: a prompt, the reply after it,
/// and a tool call each reach the screen under their own marker, so a
/// turn boundary is visible in the rendering the reader actually sees.
/// `transcript`'s own tests cover the row rules.
#[tokio::test]
async fn a_drawn_turn_shows_a_prompt_a_reply_and_a_tool_call_apart() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![
        Entry {
            kind: Some(EntryKind::User),
            raw: "hello otto".to_string(),
            ..Default::default()
        },
        Entry {
            kind: Some(EntryKind::Tool),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            tool_args: r#"{"command":"ls"}"#.to_string(),
            tool_output: "a.txt".to_string(),
            tool_done: true,
            ..Default::default()
        },
        Entry {
            kind: Some(EntryKind::Assistant),
            raw: "the reply".to_string(),
            ..Default::default()
        },
    ];

    let screen = rendered(&app, 60, 12);

    assert!(screen.contains("❯ hello otto"), "{screen}");
    assert!(screen.contains("\u{23fa} bash"), "{screen}");
    assert!(screen.contains("\u{23bf} completed · 1 lines"), "{screen}");
    assert!(
        !screen.contains("a.txt"),
        "raw output must stay folded: {screen}"
    );
    assert!(screen.contains("\u{23fa} the reply"), "{screen}");
}

/// A prompt too wide for the terminal keeps one marker and an aligned
/// indent on the rows the wrap produced, which is what `Paragraph::wrap`
/// used to lose entirely.
#[tokio::test]
async fn a_wrapped_prompt_is_marked_once_and_stays_aligned() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![Entry {
        kind: Some(EntryKind::User),
        raw: "alpha bravo charlie delta echo foxtrot golf".to_string(),
        ..Default::default()
    }];

    let rows = screen_rows(&app, 44, 10);
    let prompt: Vec<&String> = rows.iter().filter(|row| !row.is_empty()).take(2).collect();

    assert_eq!(prompt.len(), 2, "{rows:?}");
    assert!(prompt[0].starts_with("❯ "), "{rows:?}");
    assert!(prompt[1].starts_with("  "), "{rows:?}");
    assert!(!prompt[1].trim_start().starts_with('\u{276f}'), "{rows:?}");
}

/// The band is the thing a reader scrolls back to find, so it has to
/// survive the drawn frame: `Paragraph` colours only the cells a span
/// covers, and it stops at the transcript's own width, never running
/// into the side margin.
#[tokio::test]
async fn a_drawn_prompt_is_banded_across_the_content_width() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![Entry {
        kind: Some(EntryKind::User),
        raw: "hi".to_string(),
        ..Default::default()
    }];
    let (width, height) = (40u16, 10u16);

    let buffer = drawn(&app, width, height);
    let row = banded_row(&buffer, width, height).expect("the prompt row is on screen");
    for x in SIDE_MARGIN..width - SIDE_MARGIN {
        let cell = buffer.cell((x, row)).expect("cell in bounds");
        assert_eq!(
            cell.bg,
            transcript::PROMPT_BACKGROUND,
            "column {x} is not banded"
        );
    }
    for x in [0, width - 1] {
        let cell = buffer.cell((x, row)).expect("cell in bounds");
        assert_ne!(
            cell.bg,
            transcript::PROMPT_BACKGROUND,
            "the band ran into the margin"
        );
    }
}

/// The same band under wide glyphs.
///
/// The walk steps by display width, as [`super::super::selection`] does:
/// `ratatui` writes a wide character into the first of the cells it
/// covers and its frame diff never sends the rest, so `TestBackend` holds
/// a default-coloured cell where a terminal paints the glyph's own
/// background. Reading those cells back would assert on an artifact of
/// the test backend rather than on what the reader sees.
#[tokio::test]
async fn a_drawn_cjk_prompt_is_banded_too() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![Entry {
        kind: Some(EntryKind::User),
        raw: "集群数量极多".to_string(),
        ..Default::default()
    }];
    let (width, height) = (44u16, 12u16);

    let buffer = drawn(&app, width, height);
    let row = banded_row(&buffer, width, height).expect("the prompt row is on screen");
    let mut x = SIDE_MARGIN;
    while x < width - SIDE_MARGIN {
        let cell = buffer.cell((x, row)).expect("cell in bounds");
        assert_eq!(
            cell.bg,
            transcript::PROMPT_BACKGROUND,
            "column {x} breaks the band"
        );
        x += cell.symbol().width().max(1) as u16;
    }
}

#[tokio::test]
async fn narrower_or_shorter_than_the_minimum_shows_the_resize_message() {
    let (_workspace, _sessions, app) = app_fixture().await;

    let narrow = rendered(&app, MIN_TERMINAL_WIDTH - 1, MIN_TERMINAL_HEIGHT);
    let short = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT - 1);

    assert!(narrow.contains("Terminal too small"));
    assert!(short.contains("Terminal too small"));
}

#[tokio::test]
async fn exactly_the_minimum_size_shows_the_normal_layout() {
    let (_workspace, _sessions, app) = app_fixture().await;

    let content = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT);

    assert!(!content.contains("Terminal too small"));
}

#[tokio::test]
async fn normal_layout_leaves_side_margins() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system("hello from otto");
    app.input = "draft".chars().collect();
    app.cursor = app.input.len();

    let width = 80;
    let height = 20;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, &app)).expect("draw");
    let buffer = terminal.backend().buffer();

    for y in 0..height {
        assert_eq!(
            buffer.cell((0, y)).expect("left margin").symbol(),
            " ",
            "row {y} should leave the left edge empty"
        );
        assert_eq!(
            buffer.cell((width - 1, y)).expect("right margin").symbol(),
            " ",
            "row {y} should leave the right edge empty"
        );
    }
}

#[tokio::test]
async fn footer_shows_runtime_and_session_status() {
    let (_workspace, _sessions, app) = app_fixture().await;
    let session_id = app.info.session_id.clone();

    let content = rendered(&app, 160, MIN_TERMINAL_HEIGHT);

    assert!(content.contains("gpt-alpha"), "{content}");
    assert!(content.contains("default"), "{content}");
    assert!(
        content.contains("bash disabled · sandbox unavailable"),
        "{content}"
    );
    assert!(content.contains("tokens 0/0"), "{content}");
    assert!(content.contains(&session_id), "{content}");

    let narrow = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT);
    assert!(narrow.contains("gpt-alpha"), "{narrow}");
    assert!(narrow.contains("tokens 0/0"), "{narrow}");
}

/// A running task, ready to drop into `app.tasks` for the panel tests
/// below.
fn running_task(id: &str, name: &str, description: &str) -> Task {
    Task {
        id: id.into(),
        name: name.into(),
        description: description.into(),
        status: TaskStatus::Running,
        created_at: Some(chrono::Utc::now()),
        started_at: Some(chrono::Utc::now()),
        ..Task::default()
    }
}

/// Layout order top to bottom: transcript, suggestions, composer,
/// sub-agent panel, status line. With one running task the panel takes
/// one row, so the composer's bottom border, the panel row, and the
/// status line are the frame's last three rows, in that order.
#[tokio::test]
async fn the_panel_sits_between_the_composer_and_the_last_row_status_line() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = vec![running_task("t3", "review-auth", "check the callback")];

    let height = 20;
    let rows = screen_rows(&app, 100, height);
    assert_eq!(rows.len(), height as usize);

    let composer_border_row = &rows[(height - 3) as usize];
    let panel_row = &rows[(height - 2) as usize];
    let status_row = &rows[(height - 1) as usize];

    assert!(composer_border_row.contains('─'), "{composer_border_row:?}");
    assert!(
        !composer_border_row.contains("t3"),
        "{composer_border_row:?}"
    );
    assert!(panel_row.contains("t3"), "{panel_row:?}");
    assert!(panel_row.contains("review-auth"), "{panel_row:?}");
    assert!(status_row.contains("gpt-alpha"), "{status_row:?}");
}

/// The composer's empty-input floor is 1 inner row, so the box (with its
/// two border rows) is 3 rows tall.
#[tokio::test]
async fn empty_composer_is_three_rows_tall() {
    let (_workspace, _sessions, app) = app_fixture().await;
    assert_eq!(composer_height(&app, 80, COMPOSER_MIN_INNER_ROWS), 3);
}

/// With no queued or running task the panel takes no rows at all, and a
/// finished task never appears in it.
///
/// The workspace is pinned to a name that spells the task id: the status
/// line shows the workspace's last path component, and a temporary
/// directory's random name spells a two-character id often enough to have
/// failed this test's absence assertion by chance.
#[tokio::test]
async fn a_finished_task_leaves_no_panel_row() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.info.workspace = "/tmp/t9-workspace".to_string();
    app.tasks = vec![Task {
        id: "t9".into(),
        status: TaskStatus::Succeeded,
        ..Task::default()
    }];

    let height = 20;
    let rows = screen_rows(&app, 100, height);

    // No panel row means the composer's bottom border sits directly
    // above the status line.
    let composer_border_row = &rows[(height - 2) as usize];
    assert!(composer_border_row.contains('─'), "{composer_border_row:?}");
    // The pinned workspace reaches the frame, so the assertion below has
    // to exclude that row rather than pass for want of the id on screen.
    assert!(rows[(height - 1) as usize].contains("t9"), "{rows:?}");
    assert!(!above_status_line(&rows).contains("t9"), "{rows:?}");
}

/// More than 4 queued/running tasks show only the first 3 and a
/// `+N more` row for the rest.
#[tokio::test]
async fn five_running_tasks_show_three_rows_and_a_more_row() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = (0..5)
        .map(|i| running_task(&format!("t{i}"), &format!("agent{i}"), "working"))
        .collect();

    let rows = screen_rows(&app, 100, 30);
    let screen = above_status_line(&rows);

    for i in 0..3 {
        assert!(screen.contains(&format!("t{i}")), "{screen}");
    }
    for i in 3..5 {
        assert!(!screen.contains(&format!("t{i}")), "{screen}");
    }
    assert!(screen.contains("+2 more"), "{screen}");
}

/// At the minimum terminal size (40x8) with four running tasks, the rows
/// are transcript 1, composer 3, panel 3, status 1: the panel gives up
/// rows before the composer shrinks below its 1-row minimum.
#[tokio::test]
async fn the_minimum_size_keeps_the_transcript_row_and_the_status_line() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = (1..=4)
        .map(|i| running_task(&format!("t{i}"), "review", "check the diff"))
        .collect();

    let width = MIN_TERMINAL_WIDTH;
    let height = MIN_TERMINAL_HEIGHT;
    let rows = screen_rows(&app, width, height);
    assert_eq!(rows.len(), height as usize);

    // Row 0 is the transcript; the composer's titled top border is row 1
    // and its bottom border row 3, so it kept 1 inner row.
    assert!(rows[1].contains("Otto"), "{rows:?}");
    assert!(
        rows[3].contains('─') && !rows[3].contains("Otto"),
        "{rows:?}"
    );
    assert!(rows[4].contains("t1"), "{rows:?}");
    assert!(rows[5].contains("t2"), "{rows:?}");
    assert!(rows[6].contains("t3"), "{rows:?}");
    assert!(!above_status_line(&rows).contains("t4"), "{rows:?}");
    // The footer text is longer than this width, but it still starts
    // with the model field, confirming the status line landed on the
    // frame's last row rather than being pushed off it.
    assert!(rows[7].contains("gpt-alpha"), "{rows:?}");
}

/// An unnamed task is labeled by its agent, and by `default` when it has
/// no agent either.
#[test]
fn an_unnamed_panel_row_falls_back_to_the_agent() {
    let text = |task: &Task| -> String {
        panel_row_line(task, chrono::Utc::now(), 80, panel_columns(&[task]))
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    };
    let mut task = running_task("t1", "", "check the diff");
    task.agent = "reviewer".into();
    assert!(text(&task).contains("reviewer ?"), "{}", text(&task));
    task.agent.clear();
    assert!(text(&task).contains("default ?"), "{}", text(&task));
}

#[test]
fn the_panel_shows_the_model_and_live_input_output_tokens() {
    let mut task = running_task("t1", "review", "check the diff");
    task.model = "gpt-5.5".into();
    task.usage_present = true;
    task.usage.input_tokens = 12_310;
    task.usage.output_tokens = 842;

    let line = line_text(&panel_row_line(
        &task,
        chrono::Utc::now(),
        100,
        panel_columns(&[&task]),
    ));

    assert!(line.contains("gpt-5.5 in: 12.3k out:   842"), "{line}");
}

#[test]
fn panel_columns_align_across_different_name_lengths() {
    let mut short = running_task("t1", "a", "first description");
    short.model = "gpt-5.5".into();
    short.usage_present = true;
    short.usage.input_tokens = 10;
    short.usage.output_tokens = 2;
    let mut long = running_task("t2", "much-longer-name", "second description");
    long.model = "openai-compatible/model-alpha".into();
    long.usage_present = true;
    long.usage.input_tokens = 12_310;
    long.usage.output_tokens = 842;

    let columns = panel_columns(&[&short, &long]);
    let short = line_text(&panel_row_line(&short, chrono::Utc::now(), 120, columns));
    let long = line_text(&panel_row_line(&long, chrono::Utc::now(), 120, columns));

    assert!(long.contains("much-longer-name"), "{long}");
    assert!(long.contains("openai-compatible/model-alpha"), "{long}");
    assert_eq!(
        short.find("gpt-5.5"),
        long.find("openai-compatible/model-alpha")
    );
    assert_eq!(short.find("in:"), long.find("in:"));
    assert_eq!(short.find("out:"), long.find("out:"));
    assert_eq!(
        short.find("first description"),
        long.find("second description")
    );
}

/// The sub-agent panel needs the idle loop's 1-second redraw timer iff at
/// least one task is queued or running; a registry with nothing pending,
/// or only finished tasks, needs no timer.
#[test]
fn needs_task_clock_only_with_a_queued_or_running_task() {
    assert!(!needs_task_clock(&[]));
    assert!(!needs_task_clock(&[Task {
        status: TaskStatus::Succeeded,
        ..Task::default()
    }]));
    assert!(needs_task_clock(&[Task {
        status: TaskStatus::Queued,
        ..Task::default()
    }]));
    assert!(needs_task_clock(&[Task {
        status: TaskStatus::Running,
        ..Task::default()
    }]));
}

/// No-panic smoke test at extreme terminal sizes. Ratatui's `Buffer` makes
/// staying within bounds structurally true (see this test module's doc
/// comment above), so what is left to check is that drawing at these sizes
/// does not panic.
#[tokio::test]
async fn draw_does_not_panic_at_extreme_terminal_sizes() {
    let (_workspace, _sessions, app) = app_fixture().await;

    for (width, height) in [(1u16, 1u16), (2, 1), (10, 3), (39, 7)] {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw at extreme size");
    }
}

/// Confirms Otto's own transcript text — a run of wide (CJK/emoji)
/// characters and a long unbroken ASCII token with no wrap points — feeds
/// into `Paragraph::wrap` without panicking at a narrow width. This does
/// not re-test ratatui's own wrapping algorithm, only that Otto's text
/// reaches it intact.
#[tokio::test]
async fn wide_characters_and_unbroken_tokens_wrap_without_panicking() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system("测试测试测试测试测试测试测试测试测试测试😀😀😀😀😀😀😀😀😀😀😀😀");
    app.push_system("x".repeat(200));

    let backend = TestBackend::new(MIN_TERMINAL_WIDTH, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| draw(frame, &app))
        .expect("draw with wide/unbroken content");
}

#[tokio::test]
async fn manual_scroll_moves_up_from_the_bottom() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system(
        (0..20)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

    assert!(rendered(&app, MIN_TERMINAL_WIDTH, 12).contains("line 19"));
    app.scroll = Some(3);
    let scrolled = rendered(&app, MIN_TERMINAL_WIDTH, 12);
    assert!(!scrolled.contains("line 19"), "{scrolled}");
}

/// A manual scroll is an absolute top offset, so streamed output
/// appended below it leaves the rows being read exactly where they are
/// instead of pushing them off the top.
#[tokio::test]
async fn a_scrolled_view_stays_put_while_output_streams_in() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system(
        (0..20)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // Lays out `max_scroll`, which the first scroll key reads.
    let _ = rendered(&app, MIN_TERMINAL_WIDTH, 12);
    app.handle_scroll_key(&KeyCode::PageUp.into());
    let before = rendered(&app, MIN_TERMINAL_WIDTH, 12);
    assert!(before.contains("line 05"), "{before}");

    for index in 0..10 {
        app.apply_event(&wire_frame(&Event::TextDelta {
            text: format!("streamed {index}\n"),
        }));
    }

    let after = rendered(&app, MIN_TERMINAL_WIDTH, 12);
    assert!(after.contains("line 05"), "{after}");
    assert!(!after.contains("streamed 9"), "{after}");
}

/// The view half of command completion: a `/` prefix lists the matching
/// commands with their descriptions and leaves the others out.
#[tokio::test]
async fn the_selected_suggestion_row_is_highlighted() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = "/s".chars().collect();
    app.cursor = app.input.len();
    app.suggestion = 1;

    let highlighted = highlighted_rows(&app, 100, 20);

    assert_eq!(highlighted.len(), 1, "{highlighted:?}");
    assert!(
        highlighted[0].trim_start().starts_with("/sandbox"),
        "{highlighted:?}"
    );
}

/// A picker holds up to `PICKER_LIST_LIMIT` (50) rows, more than a popup
/// ever shows, so a selection below the fold has to scroll into view.
#[tokio::test]
async fn the_caret_follows_the_display_width_of_wide_characters() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = "测试ab".chars().collect();
    app.cursor = app.input.len();

    let (x, y) = cursor(&app, 100, 20);

    // One line of input fills the composer's 1-row minimum, so the box is
    // 3 rows (1 inner + 2 borders) and the caret sits on the
    // first inner row, `composer height` rows above the bottom of the
    // frame (the status line takes the last row; there is no panel row).
    assert_eq!((x, y), (SIDE_MARGIN + 1 + 6, 20 - 3));
}

/// A hard line break moves the caret to the next composer row.
#[tokio::test]
async fn the_caret_follows_an_embedded_newline() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = "ab\ncd".chars().collect();
    app.cursor = app.input.len();

    let (x, y) = cursor(&app, 100, 20);

    // Two lines grow the composer to 4 rows including borders; the caret
    // follows to the second inner row.
    assert_eq!((x, y), (SIDE_MARGIN + 1 + 2, 20 - 4 + 1));
}

/// Past [`INPUT_BOX_THRESHOLD`] rows the composer stops growing, so it
/// has to scroll: the tail of the value and the caret both stay inside
/// the box instead of the first rows being pinned and the caret
/// wandering onto the border.
#[tokio::test]
async fn a_value_taller_than_the_composer_scrolls_its_tail_into_view() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = format!("{}END", "x".repeat(96 * 13)).chars().collect();
    app.cursor = app.input.len();

    let height = 30;
    let content = rendered(&app, 100, height);
    let (x, y) = cursor(&app, 100, height);

    assert!(content.contains("END"), "{content}");
    // The box is `INPUT_BOX_THRESHOLD` text rows plus two borders. With
    // no panel row, the composer's bottom border sits directly above the
    // status line (the frame's last row), so its last text row is 3 rows
    // above the bottom of the frame.
    assert_eq!((x, y), (SIDE_MARGIN + 1 + 3, height - 3));
}

fn highlighted_rows(app: &App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    (0..height)
        .filter_map(|y| {
            let row: Vec<&ratatui::buffer::Cell> =
                (0..width).filter_map(|x| buffer.cell((x, y))).collect();
            row.iter()
                .any(|cell| cell.style().add_modifier.contains(Modifier::REVERSED))
                .then(|| row.iter().map(|cell| cell.symbol()).collect::<String>())
        })
        .collect()
}

fn cursor(app: &App, width: u16, height: u16) -> (u16, u16) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, app)).expect("draw");
    let position = terminal
        .backend_mut()
        .get_cursor_position()
        .expect("cursor position");
    (position.x, position.y)
}
