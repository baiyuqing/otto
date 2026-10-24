use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};

use super::*;

#[test]
fn clicking_readonly_document_keeps_the_approval_action_focused() {
    let view = interaction::View {
        document: "Review details".into(),
        actions: vec![
            interaction::Action {
                id: "accept".into(),
                enabled: true,
                ..Default::default()
            },
            interaction::Action {
                id: "reject".into(),
                enabled: true,
                ..Default::default()
            },
        ],
        default_action: Some("accept".into()),
        ..Default::default()
    };
    let mut state = interaction::State::new(&view);
    let geometry = widgets::Geometry {
        document: Some(ratatui::layout::Rect::new(10, 3, 30, 8)),
        actions: vec![
            ("accept".into(), ratatui::layout::Rect::new(10, 12, 15, 1)),
            ("reject".into(), ratatui::layout::Rect::new(25, 12, 15, 1)),
        ],
        ..Default::default()
    };

    focus_surface_state(&mut state, &geometry, ratatui::layout::Position::new(20, 6));

    assert_eq!(state.focus, interaction::Focus::Actions);
    assert_eq!(state.selected_action.as_deref(), Some("accept"));
}

/// The drag phases have to survive the mapping intact: a selection that
/// loses its `Down` never gets an anchor, and one that loses its `Up`
/// never copies.
#[test]
fn a_left_button_drag_maps_to_the_three_selection_phases() {
    let at = |kind| {
        map_terminal_event(TermEvent::Mouse(MouseEvent {
            kind,
            column: 7,
            row: 3,
            modifiers: KeyModifiers::NONE,
        }))
    };
    let phase = |kind| match at(kind) {
        Some(TuiEvent::Select { phase, col, row }) => {
            assert_eq!((col, row), (7, 3));
            Some(phase)
        }
        _ => None,
    };
    assert_eq!(
        phase(MouseEventKind::Down(MouseButton::Left)),
        Some(DragPhase::Start)
    );
    assert_eq!(
        phase(MouseEventKind::Drag(MouseButton::Left)),
        Some(DragPhase::Extend)
    );
    assert_eq!(
        phase(MouseEventKind::Up(MouseButton::Left)),
        Some(DragPhase::End)
    );
    assert!(
        at(MouseEventKind::Down(MouseButton::Right)).is_none(),
        "only the left button selects"
    );
    assert!(
        at(MouseEventKind::Moved).is_none(),
        "pointer motion with no button held has no behavior"
    );
}

#[test]
fn bracketed_paste_maps_to_composer_text() {
    match map_terminal_event(TermEvent::Paste("one\ntwo".to_string())) {
        Some(TuiEvent::Paste(text)) => assert_eq!(text, "one\ntwo"),
        _ => panic!("paste should be forwarded to the composer"),
    }
}

/// The release has to read the frame that is on screen.
///
/// Regression: the first version read `Terminal::current_buffer_mut()`
/// after the draw, but `swap_buffers` resets that buffer and flips to
/// it, so the release copied a blank screen while the highlight (drawn
/// inside the draw closure) looked correct.
#[tokio::test]
async fn releasing_a_drag_copies_the_text_that_is_on_screen() {
    use unicode_width::UnicodeWidthStr;

    const WIDTH: u16 = 60;
    const HEIGHT: u16 = 20;
    const NEEDLE: &str = "selectable transcript text";

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await;
    let mut app = App::new(&app::Backend::Local(&controller));
    app.push_system(NEEDLE.to_string());

    let mut terminal =
        Terminal::new(ratatui::backend::TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
    terminal
        .draw(|frame| render::draw(frame, &app))
        .expect("draw");

    // Find the needle on screen rather than assuming a layout.
    let buffer = terminal.backend().buffer().clone();
    let (col, row) = (0..HEIGHT)
        .find_map(|y| {
            let line: String = (0..WIDTH)
                .filter_map(|x| buffer.cell((x, y)))
                .map(|cell| cell.symbol())
                .collect();
            // `find` gives a byte offset; the drag needs a column, and
            // the transcript's gutter marker is not ASCII.
            line.find(NEEDLE)
                .map(|at| (UnicodeWidthStr::width(&line[..at]) as u16, y))
        })
        .expect("the transcript shows the pushed line");

    assert_eq!(
        apply_selection(&mut app, &mut terminal, DragPhase::Start, col, row).expect("draw"),
        None
    );
    assert_eq!(
        apply_selection(
            &mut app,
            &mut terminal,
            DragPhase::Extend,
            col + NEEDLE.len() as u16 - 1,
            row,
        )
        .expect("draw"),
        None
    );
    assert_eq!(
        apply_selection(
            &mut app,
            &mut terminal,
            DragPhase::End,
            col + NEEDLE.len() as u16 - 1,
            row,
        )
        .expect("draw")
        .as_deref(),
        Some(NEEDLE)
    );
}

/// A press replaces whatever was selected; a release that never moved is
/// a plain click, so it clears the highlight rather than copying a cell.
#[tokio::test]
async fn a_click_without_a_drag_clears_the_selection_and_copies_nothing() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await;
    let mut app = App::new(&app::Backend::Local(&controller));
    app.selection = Some(Selection::new(1, 1));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(40, 10)).expect("terminal");

    apply_selection(&mut app, &mut terminal, DragPhase::Start, 2, 0).expect("draw");
    assert_eq!(app.selection, Some(Selection::new(2, 0)));

    assert_eq!(
        apply_selection(&mut app, &mut terminal, DragPhase::End, 2, 0).expect("draw"),
        None,
        "a click copies nothing"
    );
    assert_eq!(app.selection, None);
}

#[test]
fn mouse_reporting_is_enabled_once_for_the_whole_session() {
    let mut output = Vec::new();
    let mut input = TerminalInput::new();

    input.enable_mouse(&mut output).expect("capture");
    let captured = String::from_utf8_lossy(&output);
    assert!(captured.contains("\u{1b}[?1000h"));
    assert!(captured.contains("\u{1b}[?1006h"));
    assert!(
        captured.contains("\u{1b}[?1002h"),
        "drag drives the selection"
    );
    assert!(
        !captured.contains("\u{1b}[?1003h"),
        "Otto reads no motion outside a drag, so do not wake on every pointer move"
    );

    let len_after_capture = output.len();
    input
        .enable_mouse(&mut output)
        .expect("capture is idempotent");
    assert_eq!(output.len(), len_after_capture);

    input.restore_mouse(&mut output).expect("release capture");
    let restored = String::from_utf8_lossy(&output);
    assert!(restored.contains("\u{1b}[?1000l"));
    assert!(restored.contains("\u{1b}[?1006l"));
}

#[test]
fn terminal_input_drop_restores_mouse_capture_only_if_it_was_enabled() {
    let mut disabled = Vec::new();
    {
        let mut input = TerminalInput::new();
        input
            .restore_mouse(&mut disabled)
            .expect("restore inactive");
    }
    assert!(!String::from_utf8_lossy(&disabled).contains("\u{1b}[?1000l"));

    let mut enabled = Vec::new();
    let mut input = TerminalInput::new();
    input.enable_mouse(&mut enabled).expect("capture");
    input.restore_mouse(&mut enabled).expect("restore active");
    assert!(String::from_utf8_lossy(&enabled).contains("\u{1b}[?1000l"));
}

/// The wheel must not reach the composer's keys: Up/Down there recall
/// prompt history, so a wheel notch mapped onto them would scroll the
/// history instead of the transcript.
#[test]
fn mouse_wheel_scrolls_without_becoming_a_composer_key() {
    let wheel = |kind| {
        let event = TermEvent::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        match map_terminal_event(event) {
            Some(TuiEvent::Wheel { up }) => Some(up),
            _ => None,
        }
    };

    assert_eq!(wheel(MouseEventKind::ScrollUp), Some(true));
    assert_eq!(wheel(MouseEventKind::ScrollDown), Some(false));
    assert!(wheel(MouseEventKind::Moved).is_none());
}

/// The reported bad experience: while a turn streamed, every key but the
/// interrupt keys was dropped, so the wheel did nothing at exactly the
/// moment there was output to scroll back through.
#[tokio::test]
async fn a_turn_scrolls_on_the_wheel_and_still_cancels_on_esc() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await;
    let mut app = App::new(&app::Backend::Local(&controller));
    app.max_scroll.set(10);
    app.start_turn();
    let turn = CancellationToken::new();

    let wheel_up = map_terminal_event(TermEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }))
    .expect("wheel event");
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(40, 10)).expect("terminal");
    apply_turn_key(
        &mut app,
        &mut terminal,
        wheel_up,
        &turn,
        &app::Backend::Local(&controller),
    );

    assert_eq!(app.scroll, Some(9));
    assert!(!turn.is_cancelled());

    apply_turn_key(
        &mut app,
        &mut terminal,
        TuiEvent::Key(KeyCode::Esc.into()),
        &turn,
        &app::Backend::Local(&controller),
    );
    assert!(turn.is_cancelled());
}

/// A provider that answers every call with `reply(call number)` and
/// records the role of each request's last message.
mod idle_provider {
    use std::sync::{Arc, Mutex};

    use otto_core::model::{Block, BlockType, FinishReason, Message, Role};
    use otto_core::provider::{
        Provider, ProviderError, Request as ProviderRequest, Response as ProviderResponse,
        StreamEvent, StreamSink,
    };

    pub(super) struct ScriptedProvider {
        reply: Box<dyn Fn(usize) -> Result<String, String> + Send + Sync>,
        roles: Mutex<Vec<Role>>,
        calls: tokio::sync::watch::Sender<usize>,
    }

    impl ScriptedProvider {
        pub(super) fn new(
            reply: impl Fn(usize) -> Result<String, String> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                reply: Box::new(reply),
                roles: Mutex::new(Vec::new()),
                calls: tokio::sync::watch::channel(0).0,
            })
        }

        pub(super) fn roles(&self) -> Vec<Role> {
            self.roles.lock().expect("roles").clone()
        }

        pub(super) fn calls(&self) -> usize {
            *self.calls.borrow()
        }

        pub(super) async fn wait_calls(&self, count: usize) {
            let mut receiver = self.calls.subscribe();
            receiver
                .wait_for(|seen| *seen >= count)
                .await
                .expect("sender");
        }
    }

    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        async fn complete(
            &self,
            request: &ProviderRequest,
            emit: StreamSink<'_>,
            _control: &dyn otto_core::operation::OperationControl,
        ) -> otto_core::provider::ProviderSettlement {
            let call = {
                let mut roles = self.roles.lock().expect("roles");
                roles.push(
                    request
                        .messages
                        .last()
                        .map(|message| message.role.clone())
                        .unwrap_or(Role::User),
                );
                roles.len()
            };
            self.calls.send_modify(|seen| *seen = call);
            let text = match (self.reply)(call) {
                Ok(text) => text,
                Err(error) => {
                    return otto_core::provider::ProviderSettlement::failed(
                        ProviderError::Other(error),
                        0,
                        otto_core::model::EffectCertainty::NotStarted,
                    );
                }
            };
            emit(StreamEvent::TextDelta { text: text.clone() });
            otto_core::provider::ProviderSettlement::succeeded(
                ProviderResponse {
                    message: Message {
                        role: Role::Assistant,
                        finish_reason: Some(FinishReason::Stop),
                        blocks: vec![Block {
                            block_type: BlockType::Text,
                            text,
                            ..Block::default()
                        }],
                        ..Message::default()
                    },
                    cached_input_tokens_present: false,
                    remote_compaction: None,
                },
                0,
            )
        }
    }
}

/// The idle loop starts a turn only when a notification is pending. A
/// registry signal with nothing pending must not start a turn; a later
/// pending notification must.
#[tokio::test]
async fn the_idle_loop_wakes_only_when_a_notification_is_pending() {
    use std::sync::Arc;
    use std::time::Duration;

    use idle_provider::ScriptedProvider;
    use otto_core::agent::inbox::Notification;
    use otto_core::model::Role;
    use otto_core::provider::Provider;
    use ratatui::backend::TestBackend;

    use crate::runtime::builder::Runner;
    use crate::subagent::tasks::Tasks;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let provider = ScriptedProvider::new(|_| Ok("woke up".to_string()));
    let builder = crate::cli::fixture_tests::builder(workspace.path(), sessions.path());
    let runtime = crate::cli::fixture_tests::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let info = builder.runtime_info(&runtime);
    let runner = Runner::scripted(
        session.clone(),
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    let controller = Controller::new(builder, true, session, runner, info);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let (_keys_tx, mut keys) = mpsc::channel(1);
    let cancel = CancellationToken::new();

    let driver = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        tasks
            .add(crate::subagent::tasks::Task::default(), None, None)
            .expect("add");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            provider.calls(),
            0,
            "a wake turn ran before any notification was pending"
        );
        tasks.notifications().push(Notification {
            task_id: "t1".to_string(),
            text: "[task-notification] task t1 succeeded".to_string(),
            ..Notification::default()
        });
        tokio::time::timeout(Duration::from_secs(2), provider.wait_calls(1))
            .await
            .expect("the pending notification did not trigger a wake turn");
        cancel.cancel();
    };
    let controller = Arc::new(controller);
    let (result, ()) = tokio::join!(
        run_app(&mut terminal, &controller, &cancel, &mut keys),
        driver
    );

    match result {
        Err(ReplError::Cancelled) => {}
        other => panic!("idle loop should end on cancel, got {other:?}"),
    }
    assert_eq!(
        provider.roles(),
        vec![Role::Context],
        "want exactly one wake turn, whose last request message is the notification"
    );
}

/// Prompts saved in the turn queue before Otto stopped run when the TUI
/// opens the session, in order, each once, carrying its input id.
#[tokio::test]
async fn saved_queued_prompts_run_in_order_when_the_tui_opens_the_session() {
    use std::sync::Arc;

    use idle_provider::ScriptedProvider;
    use otto_core::provider::Provider;
    use ratatui::backend::TestBackend;

    use crate::runtime::builder::Runner;
    use crate::subagent::tasks::Tasks;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let provider = ScriptedProvider::new(|call| Ok(format!("answer {call}")));
    let builder = crate::cli::fixture_tests::builder(workspace.path(), sessions.path());
    let runtime = crate::cli::fixture_tests::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let info = builder.runtime_info(&runtime);
    let runner = Runner::scripted(
        session.clone(),
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        Arc::clone(&tasks),
    );
    runner
        .task_control()
        .set_turn_queue_journal(Arc::new(otto_core::mailbox::InMemoryMailbox::default()));
    let controller = Arc::new(Controller::new(builder, true, session, runner, info));
    let journal = controller.turn_queue().expect("journal");
    for (input_id, text) in [("in-1", "first saved"), ("in-2", "second saved")] {
        journal
            .save(&crate::app::turn_queue::QueuedPrompt {
                input_id: input_id.into(),
                turn_id: String::new(),
                text: text.into(),
                channel: None,
                origin: None,
                image_dropped: false,
            })
            .expect("save");
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    let (_keys_tx, mut keys) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    let driver = async {
        tokio::time::timeout(std::time::Duration::from_secs(2), provider.wait_calls(2))
            .await
            .expect("the saved prompts did not run");
        cancel.cancel();
    };
    let (_, ()) = tokio::join!(
        run_app(&mut terminal, &controller, &cancel, &mut keys),
        driver
    );

    let history = controller.history();
    let users: Vec<(String, String)> = history
        .iter()
        .filter(|message| message.role == otto_core::model::Role::User)
        .map(|message| (message.text(), message.input_id.clone()))
        .collect();
    assert_eq!(
        users,
        [
            ("first saved".to_string(), "in-1".to_string()),
            ("second saved".to_string(), "in-2".to_string())
        ]
    );
    assert!(
        history.iter().any(|message| message
            .text()
            .contains("Otto stopped before 2 queued prompts")),
        "the restore notice was delivered"
    );
    assert!(journal.pending().is_empty());
    assert_eq!(provider.calls(), 2, "each ran once");
}
