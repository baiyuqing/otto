use super::*;

fn body(pairs: &[(&str, &str)]) -> toml::Table {
    let mut table = toml::Table::new();
    for (key, value) in pairs {
        table.insert((*key).into(), (*value).into());
    }
    table
}

#[test]
fn inline_tables_render_as_empty_braces_or_one_line_pairs() {
    assert_eq!(toml::Value::Table(toml::Table::new()).to_string(), "{}");
    assert_eq!(
        toml::Value::Table(body(&[("thinking", "high")])).to_string(),
        "{ thinking = \"high\" }"
    );
}

#[test]
fn has_header_is_true_only_for_tables_written_as_headers() {
    let text = "[a]\nb.c = 1\n[d.\"e.f\"]\n";
    assert!(has_header(text, &["a"]));
    assert!(has_header(text, &["d", "e.f"]));
    assert!(!has_header(text, &["a", "b"]));
    assert!(!has_header(text, &["d"]));
    assert!(!has_header(text, &[]));
}

#[test]
fn has_header_below_is_true_only_for_longer_header_paths() {
    let text = "[p.models.\"a\"]\nx = 1\n[q]\nm.n = 1\n";
    assert!(has_header_below(text, &["p", "models"]));
    assert!(has_header_below(text, &["p"]));
    assert!(!has_header_below(text, &["p", "models", "a"]));
    assert!(!has_header_below(text, &["q"]));
    assert!(!has_header_below(text, &["r"]));
}

#[test]
fn set_inline_table_inserts_after_the_last_statement_of_an_existing_block() {
    let text =
        "[providers.chatgpt]\ntype = \"chatgpt\"\nmodels.\"a\" = {}\n\n# next\n[other]\nx = 1\n";
    let updated = set_inline_table(
        text,
        &["providers", "chatgpt"],
        &["models", "gpt-5.6-sol"],
        Some(body(&[("thinking", "high")])),
    )
    .expect("insert");
    assert_eq!(
        updated,
        text.replace(
            "models.\"a\" = {}\n",
            "models.\"a\" = {}\nmodels.\"gpt-5.6-sol\" = { thinking = \"high\" }\n"
        )
    );
}

#[test]
fn set_inline_table_appends_the_header_when_it_is_missing() {
    let text = "default_model = \"chatgpt/x\"\n";
    let updated = set_inline_table(
        text,
        &["providers", "chatgpt"],
        &["models", "gpt-5.6-sol"],
        Some(toml::Table::new()),
    )
    .expect("create header");
    assert_eq!(
        updated,
        format!("{text}\n[providers.chatgpt]\nmodels.\"gpt-5.6-sol\" = {{}}\n")
    );
    let no_newline = set_inline_table("x = 1", &["p"], &["models", "m"], Some(toml::Table::new()))
        .expect("no final newline");
    assert_eq!(no_newline, "x = 1\n\n[p]\nmodels.\"m\" = {}\n");
    let empty = set_inline_table("", &["p"], &["models", "m"], Some(toml::Table::new()))
        .expect("empty text");
    assert_eq!(empty, "[p]\nmodels.\"m\" = {}\n");
}

#[test]
fn set_inline_table_replaces_the_value_and_keeps_the_trailing_comment() {
    let text = "[p]\nmodels.\"m\" = { thinking = \"low\" } # keep\nz = 1\n";
    let updated = set_inline_table(
        text,
        &["p"],
        &["models", "m"],
        Some(body(&[("thinking", "max")])),
    )
    .expect("replace");
    assert_eq!(
        updated,
        "[p]\nmodels.\"m\" = { thinking = \"max\" } # keep\nz = 1\n"
    );
}

#[test]
fn set_inline_table_removes_the_statement_and_refuses_a_missing_one() {
    let text = "[p]\nmodels.\"m\" = {}\nmodels.\"n\" = {}\n";
    assert_eq!(
        set_inline_table(text, &["p"], &["models", "m"], None).expect("remove"),
        "[p]\nmodels.\"n\" = {}\n"
    );
    assert!(set_inline_table(text, &["p"], &["models", "x"], None).is_err());
    assert!(set_inline_table(text, &["q"], &["models", "m"], None).is_err());
}

#[test]
fn set_inline_table_refuses_a_target_written_as_a_header_table() {
    let text = "[p]\n\n[p.models.\"m\"]\nthinking = \"low\"\n";
    for body in [Some(toml::Table::new()), None] {
        let error = set_inline_table(text, &["p"], &["models", "m"], body).expect_err("header");
        assert!(
            error.to_string().contains("configuration was not changed"),
            "{error}"
        );
    }
}

#[test]
fn set_value_edits_a_key_inside_an_inline_table_statement() {
    let text = "[a]\nb.\"c.d\" = { x = 1 } # note\n";
    let updated = set_value(text, &["a", "b", "c.d"], "y", Some("2")).expect("set");
    assert_eq!(updated, "[a]\nb.\"c.d\" = { x = 1, y = 2 } # note\n");
    let removed = set_value(&updated, &["a", "b", "c.d"], "x", None).expect("remove");
    assert_eq!(removed, "[a]\nb.\"c.d\" = { y = 2 } # note\n");
    let emptied = set_value(&removed, &["a", "b", "c.d"], "y", None).expect("remove last");
    assert_eq!(emptied, "[a]\nb.\"c.d\" = {} # note\n");
    assert!(set_value(text, &["a", "b", "other"], "y", Some("2")).is_err());
}
