//! Tests for feature panels drawn over the session shell.

use super::*;

#[tokio::test]
async fn every_feature_draws_as_a_centered_panel_over_the_session_shell() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries.push(Entry {
        kind: Some(EntryKind::User),
        raw: "underlying conversation".into(),
        ..Entry::default()
    });
    let remote = std::sync::Arc::new(crate::tui::attach::Remote::offline(Vec::new()));
    let backend = super::super::super::app::Backend::Attach(&remote);
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

#[test]
fn skill_manager_renders_selection_preview_and_scrolling_in_small_terminals() {
    use crate::tui::features::Presenter;
    use crate::tui::interaction::Intent;
    let remote = crate::tui::attach::Remote::offline(Vec::new());
    let mut app = App::new(&crate::tui::app::Backend::Attach(&remote));
    let mut skills = crate::tui::features::skills::Skills::default();
    let rows = (0..80)
        .map(|i| otto::app::management::ManagedSkill {
            skill: otto::skill::Skill {
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
        otto::app::management::Reply::SkillManager(otto::app::management::SkillManager {
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
