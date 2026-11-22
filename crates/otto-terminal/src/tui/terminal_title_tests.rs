use super::parse_title;

#[test]
fn only_replay_well_formed_titles_without_terminal_controls() {
    assert_eq!(parse_title(br#"{"title":""}"#).as_deref(), Some(""));
    assert_eq!(
        parse_title("{\"title\":\"项目; shell\"}".as_bytes()).as_deref(),
        Some("项目; shell")
    );
    for invalid in [
        &br#"{}"#[..],
        br#"{"title":null}"#,
        br#"{"title":"before\u001b\\after"}"#,
        br#"{"title":"before\u0007after"}"#,
        br#"{"title":"before\u009cafter"}"#,
    ] {
        assert_eq!(parse_title(invalid), None);
    }
}
