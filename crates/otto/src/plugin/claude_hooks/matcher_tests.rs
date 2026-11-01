use super::matcher::Matcher;

fn parsed(text: &str) -> Matcher {
    Matcher::parse(text).unwrap()
}

#[test]
fn empty_and_star_match_everything() {
    for text in ["", "*"] {
        let matcher = parsed(text);
        assert!(matches!(matcher, Matcher::Any));
        assert!(matcher.matches("Edit") && matcher.matches(""));
    }
}

#[test]
fn name_lists_match_exactly() {
    for text in ["Edit|Write", "Edit, Write", "Edit,Write", " Edit | Write "] {
        let matcher = parsed(text);
        assert!(matches!(matcher, Matcher::Names(_)), "{text}");
        assert!(
            matcher.matches("Edit") && matcher.matches("Write"),
            "{text}"
        );
        assert!(
            !matcher.matches("EditFile") && !matcher.matches("Read"),
            "{text}"
        );
    }
    let single = parsed("Bash");
    assert!(single.matches("Bash") && !single.matches("Bash2") && !single.matches("bash"));
}

#[test]
fn names_with_internal_spaces_stay_one_name() {
    let matcher = parsed("Edit File|Write");
    assert!(matcher.matches("Edit File") && matcher.matches("Write"));
    assert!(!matcher.matches("Edit") && !matcher.matches("File"));
}

#[test]
fn separators_only_list_matches_nothing() {
    let matcher = parsed("|, ");
    assert!(matches!(&matcher, Matcher::Names(names) if names.is_empty()));
    assert!(!matcher.matches("") && !matcher.matches("Edit"));
}

#[test]
fn other_text_is_an_unanchored_case_sensitive_regex() {
    let matcher = parsed("^mcp__");
    assert!(matches!(matcher, Matcher::Pattern(_)));
    assert!(matcher.matches("mcp__demo__read"));
    assert!(!matcher.matches("tool_mcp__x"));
    let unanchored = parsed("Notebook.*");
    assert!(unanchored.matches("xNotebookEdit") && !unanchored.matches("notebook"));
}

#[test]
fn invalid_or_oversized_text_is_an_error() {
    assert!(Matcher::parse("(").is_err());
    assert!(Matcher::parse(&"a".repeat(1025)).is_err());
    assert!(Matcher::parse(&"a".repeat(1024)).is_ok());
}
