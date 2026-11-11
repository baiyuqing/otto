use super::*;

fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

#[test]
fn raw_tail_keeps_tool_batches_and_images_intact() {
    let mut messages = vec![
        message(Role::User, "old"),
        message(Role::Assistant, "call"),
        message(Role::Tool, "result1"),
        message(Role::Tool, "result2"),
    ];
    assert_eq!(retained_tail_start(&messages, 1), 1);
    messages[0].blocks.push(Block {
        block_type: BlockType::Image,
        ..Block::default()
    });
    assert_eq!(retained_tail_start(&messages, 1), 0);
}

#[test]
fn zoom_preserves_every_source_byte_and_rejects_overflow() {
    let context = ContextView::default();
    let source = message(Role::Context, &"内容🙂".repeat(5000));
    let encoded = serde_json::to_string(&source).unwrap();
    context.synchronize(&[source], 1000).unwrap();
    let chunks = context.lock().log.len();
    let mut reconstructed = String::new();
    for id in 0..chunks as u64 {
        reconstructed.push_str(
            context
                .zoom(id, 1)
                .unwrap()
                .strip_prefix("context: ")
                .unwrap(),
        );
    }
    assert_eq!(reconstructed, encoded);
    for (id, n) in [(u64::MAX, 1), (0, 0), (1, 2), (0, 3), (chunks as u64, 1)] {
        assert!(context.zoom(id, n).is_err());
    }
}

#[test]
fn changing_source_invalidates_tree_and_cannot_project_a_different_prefix() {
    let context = ContextView::default();
    let source = message(Role::User, "original");
    context.synchronize(&[source.clone()], 1000).unwrap();
    {
        let mut state = context.lock();
        let address = crate::view_tree::NodeAddress::leaf(0);
        state.tree.insert(address, "user: original".into()).unwrap();
        state.view.append(0);
        state.ready = true;
    }
    assert_eq!(context.project(vec![source])[0].context_type, "view_tree");
    let replacement = message(Role::User, "edited");
    assert_eq!(
        context.project(vec![replacement.clone()])[0].text(),
        "edited"
    );
    context.synchronize(&[replacement], 1000).unwrap();
    assert!(context.lock().tree.is_empty());
    assert!(!context.zoom(0, 1).unwrap().contains("original"));
}
