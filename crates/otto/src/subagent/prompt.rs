//! The `## Agents` system-prompt section.
//!
//! The escaping and whitespace rules are shared with the skills section, so
//! this module reuses [`crate::skill::prompt`]'s helpers rather than repeating
//! them. A definition that defaults to `context: inherit` carries the
//! attribute, because the `agent` tool's `context` parameter is optional and
//! the model otherwise cannot tell which definitions copy the conversation.

use super::Catalog;
use crate::skill::prompt::{collapse_whitespace, escape_html};

/// Caps the rendered prompt section, the same cap the skills section uses.
pub const MAX_LISTING_BYTES: usize = 8 << 10;

const AGENTS_HEADER: &str = concat!(
    "\n\n## Agents\n",
    "Named sub-agent definitions for the `agent` tool (`agent` parameter):\n",
    "<available_agents>\n",
);

/// No trailing newline: the footer ends the section exactly here.
const AGENTS_FOOTER: &str = "</available_agents>";

/// Renders the section, or `""` when `catalog` is empty. Entries that would
/// push the section past [`MAX_LISTING_BYTES`] are dropped, with one warning
/// per dropped definition.
pub fn prompt_section(catalog: &Catalog) -> (String, Vec<String>) {
    let definitions = catalog.definitions();
    if definitions.is_empty() {
        return (String::new(), Vec::new());
    }

    let mut body = String::new();
    let mut warnings = Vec::new();
    for (index, definition) in definitions.iter().enumerate() {
        // The name is already constrained to `[a-z0-9-]`; escape it anyway so
        // every attribute value goes through the same path.
        let entry = format!(
            "<agent name=\"{}\"{}>{}</agent>\n",
            escape_html(&definition.name),
            // `context` is normalised to `fresh` or `inherit` at parse time, so
            // the default costs no bytes in the listing.
            if definition.context == "inherit" {
                " context=\"inherit\""
            } else {
                ""
            },
            escape_html(&collapse_whitespace(&definition.description))
        );
        if AGENTS_HEADER.len() + body.len() + entry.len() + AGENTS_FOOTER.len() > MAX_LISTING_BYTES
        {
            warnings.extend(definitions[index..].iter().map(|remaining| {
                format!(
                    "agent {} omitted from prompt: listing exceeds {MAX_LISTING_BYTES} bytes",
                    remaining.name
                )
            }));
            break;
        }
        body.push_str(&entry);
    }
    (format!("{AGENTS_HEADER}{body}{AGENTS_FOOTER}"), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A YAML literal block, so the description can carry newlines.
    fn write_agent_description(root: &Path, name: &str, description: &str) -> PathBuf {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
        let mut content = format!("---\nname: {name}\ndescription: |\n");
        for line in description.split('\n') {
            content.push_str("  ");
            content.push_str(line);
            content.push('\n');
        }
        content.push_str("---\nbody\n");
        std::fs::write(directory.join("AGENT.md"), content).expect("AGENT.md is writable");
        directory
    }

    fn write_agent(root: &Path, name: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
        std::fs::write(
            directory.join("AGENT.md"),
            format!("---\nname: {name}\ndescription: desc for {name}\n---\nbody\n"),
        )
        .expect("AGENT.md is writable");
    }

    fn write_inheriting_agent(root: &Path, name: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
        std::fs::write(
            directory.join("AGENT.md"),
            format!(
                "---\nname: {name}\ndescription: desc for {name}\ncontext: inherit\n---\nbody\n"
            ),
        )
        .expect("AGENT.md is writable");
    }

    #[test]
    fn an_empty_catalog_renders_nothing() {
        assert_eq!(
            prompt_section(&Catalog::default()),
            (String::new(), Vec::new())
        );
    }

    #[test]
    fn two_definitions_render_the_exact_pinned_string() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_agent(root.path(), "alpha");
        write_agent(root.path(), "beta");
        let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
        assert!(warnings.is_empty(), "warnings = {warnings:?}");

        let (section, prompt_warnings) = prompt_section(&catalog);

        assert!(prompt_warnings.is_empty(), "{prompt_warnings:?}");
        assert_eq!(
            section,
            concat!(
                "\n\n## Agents\n",
                "Named sub-agent definitions for the `agent` tool (`agent` parameter):\n",
                "<available_agents>\n",
                "<agent name=\"alpha\">desc for alpha</agent>\n",
                "<agent name=\"beta\">desc for beta</agent>\n",
                "</available_agents>",
            )
        );
    }

    /// A definition that defaults to inheriting the parent conversation is
    /// marked in the listing. Without the marker the model cannot tell what
    /// omitting the tool's `context` parameter costs.
    #[test]
    fn a_definition_that_defaults_to_inherit_is_marked() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_agent(root.path(), "alpha");
        write_inheriting_agent(root.path(), "beta");
        let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
        assert!(warnings.is_empty(), "warnings = {warnings:?}");

        let (section, prompt_warnings) = prompt_section(&catalog);

        assert!(prompt_warnings.is_empty(), "{prompt_warnings:?}");
        assert_eq!(
            section,
            concat!(
                "\n\n## Agents\n",
                "Named sub-agent definitions for the `agent` tool (`agent` parameter):\n",
                "<available_agents>\n",
                "<agent name=\"alpha\">desc for alpha</agent>\n",
                "<agent name=\"beta\" context=\"inherit\">desc for beta</agent>\n",
                "</available_agents>",
            )
        );
    }

    #[test]
    fn a_description_is_escaped_and_its_whitespace_collapsed() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_agent_description(root.path(), "x", "Review <diffs> & \"reports\"");
        let (escaping, _) = Catalog::discover(&[root.path().to_path_buf()]);
        let collapsing_root = tempfile::tempdir().expect("a temporary directory");
        write_agent_description(collapsing_root.path(), "x", "line one\nline   two");
        let (collapsing, _) = Catalog::discover(&[collapsing_root.path().to_path_buf()]);

        let (escaped, _) = prompt_section(&escaping);
        let (collapsed, _) = prompt_section(&collapsing);

        assert!(
            escaped.contains("Review &lt;diffs&gt; &amp; &#34;reports&#34;</agent>"),
            "{escaped:?}"
        );
        assert!(!escaped.contains("<diffs>"), "{escaped:?}");
        assert!(
            collapsed.contains("line one line two</agent>"),
            "{collapsed:?}"
        );
    }

    #[test]
    fn the_byte_cap_drops_later_definitions_with_one_warning_each() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let long = "a".repeat(1024);
        let names = [
            "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        ];
        for name in names {
            write_agent_description(root.path(), name, &long);
        }
        let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
        assert!(warnings.is_empty(), "warnings = {warnings:?}");
        assert_eq!(catalog.len(), names.len());

        let (section, prompt_warnings) = prompt_section(&catalog);

        assert!(section.len() <= MAX_LISTING_BYTES, "{}", section.len());
        assert!(!prompt_warnings.is_empty());
        for warning in &prompt_warnings {
            assert!(warning.contains("omitted from prompt"), "{warning}");
        }
    }
}
