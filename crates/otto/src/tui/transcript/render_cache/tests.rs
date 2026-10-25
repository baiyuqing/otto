use std::time::Instant;

use super::*;
use crate::tui::entries::EntryKind;
use crate::tui::transcript;

fn text_entry(id: &str, kind: EntryKind, raw: &str) -> Entry {
    Entry {
        id: id.to_string(),
        kind: Some(kind),
        raw: raw.to_string(),
        ..Entry::default()
    }
}

fn tool_entry(id: &str, done: bool) -> Entry {
    Entry {
        id: id.to_string(),
        kind: Some(EntryKind::Tool),
        tool_call_id: "call-1".to_string(),
        tool_name: "read".to_string(),
        tool_args: r#"{"path":"src/main.rs"}"#.to_string(),
        tool_output: if done {
            "line one\nline two".into()
        } else {
            String::new()
        },
        tool_done: done,
        ..Entry::default()
    }
}

fn approval_entry() -> Entry {
    Entry {
        id: "approval-entry".to_string(),
        approval: Some(otto_core::approval::Request {
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
        }),
        ..Entry::default()
    }
}

/// Syncs `cache` and asserts every row, including span styles, equals the
/// pure function's output.
fn sync_and_compare(cache: &mut RenderCache, entries: &[Entry], details: bool, width: usize) {
    cache.sync(entries, details, width);
    let expected = transcript::lines(entries, details, width);
    assert_eq!(cache.row_count(), expected.len());
    assert_eq!(cache.rows(0..cache.row_count()), expected);
}

#[test]
fn cached_rows_equal_the_pure_function_through_mutations() {
    let mut cache = RenderCache::default();
    let mut entries: Vec<Entry> = Vec::new();
    sync_and_compare(&mut cache, &entries, false, 40);

    entries.push(text_entry(
        "u1",
        EntryKind::User,
        "hello there\nsecond line",
    ));
    sync_and_compare(&mut cache, &entries, false, 40);

    entries.push(text_entry("a1", EntryKind::Assistant, "# Title\n\nfirst"));
    sync_and_compare(&mut cache, &entries, false, 40);
    for chunk in [
        " **bold** words that wrap around the width",
        "\n\n- item",
        "\n```rust\nlet x = 1;\n```",
    ] {
        entries[1].raw.push_str(chunk);
        sync_and_compare(&mut cache, &entries, false, 40);
    }

    entries.push(tool_entry("t1", false));
    sync_and_compare(&mut cache, &entries, false, 40);
    entries[2].tool_output = "line one\nline two".into();
    entries[2].tool_done = true;
    sync_and_compare(&mut cache, &entries, false, 40);
    sync_and_compare(&mut cache, &entries, true, 40);
    sync_and_compare(&mut cache, &entries, false, 40);

    entries.insert(1, approval_entry());
    sync_and_compare(&mut cache, &entries, false, 40);

    // Empty rendering between two non-empty entries adds no row or separator.
    entries.insert(2, text_entry("empty", EntryKind::Assistant, ""));
    sync_and_compare(&mut cache, &entries, false, 40);

    sync_and_compare(&mut cache, &entries, false, 17);
    sync_and_compare(&mut cache, &entries, false, 80);

    entries.truncate(2);
    sync_and_compare(&mut cache, &entries, false, 80);
    entries = vec![text_entry("only", EntryKind::System, "short")];
    sync_and_compare(&mut cache, &entries, false, 80);
    entries.clear();
    sync_and_compare(&mut cache, &entries, false, 80);
}

#[test]
fn only_empty_entries_render_no_rows() {
    let mut cache = RenderCache::default();
    let entries = vec![
        text_entry("a", EntryKind::Assistant, ""),
        text_entry("b", EntryKind::User, ""),
    ];
    sync_and_compare(&mut cache, &entries, false, 40);
    assert_eq!(cache.row_count(), 0);
}

#[test]
fn partial_windows_equal_slices_of_the_full_list() {
    let mut cache = RenderCache::default();
    let entries = vec![
        text_entry("u1", EntryKind::User, "one\ntwo\nthree"),
        text_entry("empty", EntryKind::Assistant, ""),
        text_entry("a1", EntryKind::Assistant, "alpha\n\nbeta\n\ngamma"),
        tool_entry("t1", true),
        text_entry("e1", EntryKind::Error, "boom"),
    ];
    cache.sync(&entries, false, 30);
    let full = transcript::lines(&entries, false, 30);
    let total = full.len();
    assert!(total > 12);
    let ranges = [
        0..0,
        0..1,
        0..total,
        1..4,
        2..9,
        3..4,
        total - 1..total,
        total - 3..total + 10,
        5..total + 100,
        total..total + 5,
        total + 5..total + 9,
        Range { start: 7, end: 3 },
    ];
    for range in ranges {
        let expected: Vec<Line<'static>> = full
            .iter()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .cloned()
            .collect();
        assert_eq!(cache.rows(range.clone()), expected, "range {range:?}");
    }
    // Every window of height 4 matches, which crosses each separator.
    for start in 0..total {
        let expected: Vec<Line<'static>> = full.iter().skip(start).take(4).cloned().collect();
        assert_eq!(cache.rows(start..start + 4), expected, "start {start}");
    }
}

#[test]
fn unchanged_entries_are_not_rendered_again() {
    let mut cache = RenderCache::default();
    let mut entries = vec![
        text_entry("u1", EntryKind::User, "hi"),
        text_entry("a1", EntryKind::Assistant, "reply"),
        tool_entry("t1", true),
    ];
    cache.sync(&entries, false, 40);
    assert_eq!(cache.rendered_count(), 3);

    cache.sync(&entries, false, 40);
    assert_eq!(cache.rendered_count(), 3, "identical input re-rendered");

    entries[1].raw.push_str(" and more");
    cache.sync(&entries, false, 40);
    assert_eq!(
        cache.rendered_count(),
        4,
        "only the appended entry re-renders"
    );

    cache.sync(&entries, false, 41);
    assert_eq!(
        cache.rendered_count(),
        7,
        "a width change re-renders every entry"
    );

    cache.sync(&entries, true, 41);
    assert_eq!(
        cache.rendered_count(),
        10,
        "a details toggle re-renders every entry"
    );

    entries.truncate(2);
    cache.sync(&entries, true, 41);
    assert_eq!(cache.rendered_count(), 10, "truncation re-renders nothing");
}

#[test]
fn a_running_tool_entry_is_rendered_on_every_sync() {
    let mut cache = RenderCache::default();
    let mut running = tool_entry("t1", false);
    running.tool_started = Some(Instant::now());
    let mut entries = vec![text_entry("u1", EntryKind::User, "hi"), running];
    cache.sync(&entries, false, 40);
    assert_eq!(cache.rendered_count(), 2);
    cache.sync(&entries, false, 40);
    assert_eq!(
        cache.rendered_count(),
        3,
        "only the running tool re-renders"
    );
    cache.sync(&entries, true, 40);
    cache.sync(&entries, true, 40);
    assert_eq!(cache.rendered_count(), 6);

    // Once the elapsed time is recorded the entry no longer reads the clock.
    entries[1].tool_elapsed = Some(std::time::Duration::from_millis(1500));
    entries[1].tool_done = true;
    cache.sync(&entries, true, 40);
    let after_finish = cache.rendered_count();
    cache.sync(&entries, true, 40);
    assert_eq!(cache.rendered_count(), after_finish);
}
