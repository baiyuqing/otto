//! View-tree strategy with images: local compaction removes images the tree cannot hold.

use super::*;
use crate::agent::context_view::ContextView;
use crate::config::context_strategy::ContextStrategy;

use super::context_view::{ViewExecutor, ViewProvider, view_options};

fn image_options(
    context: Arc<ContextView>,
    working_window: i64,
    hard_input_window: i64,
) -> Options {
    let mut options = view_options(context);
    options.compaction = CompactionSettings {
        auto: false,
        hard_input_window,
        working_window,
        reserve_tokens: 0,
        keep_recent_tokens: 40,
    };
    options
}

/// An image user message followed by text turns; the image is the oldest message.
async fn image_session(text_turns: usize) -> MemorySession {
    let session = MemorySession::new();
    session
        .append(Message {
            id: "image-user".into(),
            role: Role::User,
            blocks: vec![
                Block::text("look at this"),
                Block::image("iVBORw0KGgo=", "image/png"),
            ],
            ..Message::default()
        })
        .await
        .unwrap();
    for index in 0..text_turns {
        session
            .append(Message {
                id: format!("user-{index}"),
                role: Role::User,
                blocks: vec![Block::text(format!("question {index} {}", "x".repeat(200)))],
                ..Message::default()
            })
            .await
            .unwrap();
        session
            .append(Message {
                id: format!("assistant-{index}"),
                role: Role::Assistant,
                blocks: vec![Block::text(format!("answer {index} {}", "y".repeat(200)))],
                ..Message::default()
            })
            .await
            .unwrap();
    }
    session
}

fn request_has_image(request: &Request) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .any(|block| block.block_type == BlockType::Image)
}

fn agent_with(
    turns: Vec<Turn>,
    session: MemorySession,
    options: Options,
) -> Agent<ViewProvider, ViewExecutor, MemorySession> {
    let context = options.context_view.clone();
    Agent::new(
        ViewProvider::new(turns),
        ViewExecutor { context },
        session,
        options,
    )
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_image_over_the_soft_trigger_is_compacted_locally_even_when_auto_is_off() {
    let context = Arc::new(ContextView::default());
    let agent = agent_with(
        vec![Turn::summary(&structured_summary()), Turn::text("ok")],
        image_session(3).await,
        image_options(context, 600, 4_000),
    );
    assert_eq!(agent.options.context_strategy, ContextStrategy::ViewTree);
    let mut state = super::super::overflow::RunDispatchState::default();
    agent
        .dispatch_normal_provider_step(&mut |_| {}, &mut state, "", &CancellationToken::new())
        .await
        .unwrap();
    assert!(agent.session.latest_compaction().is_some());
    assert!(agent.session.remote_compaction().is_none());
    let requests = agent.provider.ordinary.normal_requests();
    assert_eq!(requests.len(), 1);
    assert!(!request_has_image(&requests[0]));
    assert!(state.proactive_attempted);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_image_under_the_soft_trigger_stays_in_the_raw_request() {
    let context = Arc::new(ContextView::default());
    let agent = agent_with(
        vec![Turn::text("ok")],
        image_session(1).await,
        image_options(context, 50_000, 100_000),
    );
    let mut state = super::super::overflow::RunDispatchState::default();
    agent
        .dispatch_normal_provider_step(&mut |_| {}, &mut state, "", &CancellationToken::new())
        .await
        .unwrap();
    assert!(agent.session.latest_compaction().is_none());
    assert!(agent.provider.ordinary.summary_requests().is_empty());
    let requests = agent.provider.ordinary.normal_requests();
    assert!(request_has_image(&requests[0]));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn text_history_over_the_soft_trigger_uses_only_the_tree() {
    let context = Arc::new(ContextView::default());
    let session = image_session(6).await;
    let mut messages = session.model_messages();
    // The same history without the image block.
    messages[0]
        .blocks
        .retain(|block| block.block_type != BlockType::Image);
    let text_session = MemorySession::new();
    for message in messages {
        text_session.append(message).await.unwrap();
    }
    let agent = agent_with(
        vec![Turn::text("ok")],
        text_session,
        image_options(context, 600, 4_000),
    );
    let mut state = super::super::overflow::RunDispatchState::default();
    agent
        .dispatch_normal_provider_step(&mut |_| {}, &mut state, "", &CancellationToken::new())
        .await
        .unwrap();
    assert!(agent.session.latest_compaction().is_none());
    assert!(agent.provider.compressions.load(Ordering::SeqCst) > 0);
    assert!(!state.proactive_attempted);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_typed_overflow_with_an_image_compacts_locally_and_retries_once() {
    let context = Arc::new(ContextView::default());
    // Soft trigger above the image estimate, so only the overflow path compacts.
    let agent = agent_with(
        vec![
            Turn::overflow(),
            Turn::summary(&structured_summary()),
            Turn::text("ok"),
        ],
        image_session(3).await,
        image_options(context, 50_000, 100_000),
    );
    let mut state = super::super::overflow::RunDispatchState::default();
    agent
        .dispatch_normal_provider_step(&mut |_| {}, &mut state, "", &CancellationToken::new())
        .await
        .unwrap();
    assert!(agent.session.latest_compaction().is_some());
    assert!(agent.session.remote_compaction().is_none());
    let requests = agent.provider.ordinary.normal_requests();
    assert_eq!(requests.len(), 2);
    assert!(request_has_image(&requests[0]));
    assert!(!request_has_image(&requests[1]));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn manual_compaction_in_view_mode_is_local_even_with_remote_support() {
    let context = Arc::new(ContextView::default());
    let agent = agent_with(
        vec![Turn::summary(&structured_summary())],
        image_session(3).await,
        image_options(context, 50_000, 100_000),
    );
    assert!(matches!(
        agent.provider.capabilities().remote_compaction,
        RemoteCompactionSupport::ResponsesV2
    ));
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert!(agent.session.latest_compaction().is_some());
    assert!(agent.session.remote_compaction().is_none());
    assert_eq!(agent.provider.ordinary.summary_requests().len(), 1);
}
