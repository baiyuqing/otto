use super::commands::CommandGroup;
use super::{Catalog, Transition};
use crate::app::Info;

#[test]
fn catalog_lists_commands_by_workflow_group() {
    let catalog = Catalog::default();
    let entries = catalog.entries();
    assert!(
        entries
            .windows(2)
            .all(|pair| pair[0].group <= pair[1].group)
    );
    let names: Vec<&str> = entries.iter().map(|entry| entry.name).collect();
    assert_eq!(
        names,
        [
            "/help",
            "/init",
            "/login",
            "/logout",
            "/mcp",
            "/skill",
            "/model",
            "/thinking",
            "/context",
            "/compact",
            "/permissions",
            "/approve",
            "/sandbox",
            "/memory",
            "/remember",
            "/task",
            "/agents",
            "/timers",
            "/new",
            "/clear",
            "/resume",
            "/fork",
            "/rename",
            "/archive",
            "/session",
            "/exit",
        ]
    );
    assert_eq!(entries[0].group, CommandGroup::Help);
    assert_eq!(entries[25].group, CommandGroup::Exit);
}

#[test]
fn help_prints_group_headings_in_order_with_blank_line_separators() {
    let Transition::Push(presenter) = Catalog::default()
        .open("/help", &Info::default(), false)
        .unwrap()
    else {
        panic!("help should open a document");
    };
    let help = presenter.view().document;
    let headings = [
        "Help",
        "Setup",
        "During a task",
        "Approvals and sandbox",
        "Memory",
        "Tasks",
        "Sessions",
        "Exit",
    ];
    let mut cursor = 0;
    for heading in headings {
        let found = help[cursor..]
            .find(&format!("\n{heading}\n"))
            .map(|at| at + cursor)
            .or_else(|| help.starts_with(&format!("{heading}\n")).then_some(0))
            .unwrap_or_else(|| panic!("missing heading {heading}: {help}"));
        assert!(found >= cursor, "{heading} out of order: {help}");
        cursor = found + heading.len();
    }
    assert!(
        help.starts_with("Help\n/help  show commands\n\nSetup\n/init  "),
        "{help}"
    );
    let position = |name: &str| help.find(&format!("\n{name}  ")).unwrap();
    assert!(position("/init") < position("/permissions"));
    assert!(position("/permissions") < position("/memory"));
    assert!(position("/memory") < position("/task"));
    assert!(position("/task") < position("/new"));
    assert!(position("/new") < position("/exit"));
}
