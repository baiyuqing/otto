//! The `## Agents` system-prompt section.
//!
//! The escaping and whitespace rules are shared with the skills section, so
//! this module reuses [`crate::skill::prompt`]'s helpers rather than repeating
//! them. A definition that defaults to `context: inherit` carries the
//! attribute, because the `agent` tool's `context` parameter is optional and
//! the model otherwise cannot tell which definitions copy the conversation.

use super::{Catalog, Definition};
use crate::skill::prompt::{collapse_whitespace, escape_html, truncate_chars};

/// Caps the rendered prompt section, the same cap the skills section uses.
pub const MAX_LISTING_BYTES: usize = 8 << 10;

/// The number of description characters a truncated entry keeps.
const SHORT_DESCRIPTION_CHARS: usize = 120;

/// How much of every entry the listing shows, mirroring the skills section.
///
/// A definition missing from the listing is unreachable: the `agent` tool's
/// `agent` parameter is keyed by name. So an oversized catalog loses detail
/// rather than losing definitions, uniformly across entries. There is no
/// location rung here because an `<agent>` entry carries no location, and
/// `context="inherit"` survives every level: it changes what the `agent` tool
/// does, so dropping it would misreport the definition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    Full,
    ShortDescription,
    NameOnly,
}

impl Detail {
    /// Most detailed first. The last level is the fallback when none fits.
    const LADDER: [Self; 3] = [Self::Full, Self::ShortDescription, Self::NameOnly];

    /// The phrase the degradation warning uses.
    fn label(self) -> &'static str {
        match self {
            Self::Full => "full entries",
            Self::ShortDescription => "names and truncated descriptions",
            Self::NameOnly => "names only",
        }
    }
}

const AGENTS_HEADER: &str = concat!(
    "\n\n## Agents\n",
    "Named sub-agent definitions for the `agent` tool (`agent` parameter):\n",
    "<available_agents>\n",
);

/// No trailing newline: the footer ends the section exactly here.
const AGENTS_FOOTER: &str = "</available_agents>";

/// Renders the section, or `""` when `catalog` is empty.
///
/// Every entry is rendered at the most detailed [`Detail`] level whose whole
/// listing fits [`MAX_LISTING_BYTES`], so a large catalog stays complete and
/// one warning names the level it fell back to. Only a catalog too large even
/// for bare names loses entries, and that warning names them.
pub fn prompt_section(catalog: &Catalog) -> (String, Vec<String>) {
    let definitions = catalog.definitions();
    if definitions.is_empty() {
        return (String::new(), Vec::new());
    }
    let budget = MAX_LISTING_BYTES.saturating_sub(AGENTS_HEADER.len() + AGENTS_FOOTER.len());

    let mut detail = Detail::NameOnly;
    let mut body = String::new();
    for level in Detail::LADDER {
        detail = level;
        body = definitions
            .iter()
            .map(|definition| render_entry(definition, level))
            .collect();
        if body.len() <= budget {
            break;
        }
    }

    let mut warnings = Vec::new();
    if body.len() > budget {
        let mut dropped: Vec<&str> = Vec::new();
        body.clear();
        for (index, definition) in definitions.iter().enumerate() {
            let entry = render_entry(definition, Detail::NameOnly);
            if body.len() + entry.len() > budget {
                dropped.extend(
                    definitions[index..]
                        .iter()
                        .map(|definition| definition.name.as_str()),
                );
                break;
            }
            body.push_str(&entry);
        }
        warnings.push(format!(
            "agents listing exceeds {MAX_LISTING_BYTES} bytes even with names only; dropped: {}",
            dropped.join(", ")
        ));
    } else if detail != Detail::Full {
        warnings.push(format!(
            "agents listing exceeds {MAX_LISTING_BYTES} bytes; shortened to {}",
            detail.label()
        ));
    }
    (format!("{AGENTS_HEADER}{body}{AGENTS_FOOTER}"), warnings)
}

/// One `<agent>` line at `detail`. Truncation happens before escaping, so a
/// cut can never split an entity.
fn render_entry(definition: &Definition, detail: Detail) -> String {
    let description = match detail {
        Detail::Full => escape_html(&collapse_whitespace(&definition.description)),
        Detail::ShortDescription => escape_html(&truncate_chars(
            &collapse_whitespace(&definition.description),
            SHORT_DESCRIPTION_CHARS,
        )),
        Detail::NameOnly => String::new(),
    };
    // The name is already constrained to `[a-z0-9-]`; escape it anyway so
    // every attribute value goes through the same path.
    format!(
        "<agent name=\"{}\"{}>{description}</agent>\n",
        escape_html(&definition.name),
        // `context` is normalised to `fresh` or `inherit` at parse time, so
        // the default costs no bytes in the listing.
        if definition.context == "inherit" {
            " context=\"inherit\""
        } else {
            ""
        },
    )
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

    /// `count` definitions whose descriptions are `description_chars` long.
    fn catalog_of(root: &Path, count: usize, description_chars: usize) -> Catalog {
        let long = "a".repeat(description_chars);
        for index in 0..count {
            write_agent_description(root, &format!("agent-{index}"), &long);
        }
        let (catalog, warnings) = Catalog::discover(&[root.to_path_buf()]);
        assert!(warnings.is_empty(), "warnings = {warnings:?}");
        assert_eq!(catalog.len(), count);
        catalog
    }

    #[test]
    fn every_definition_stays_listed_when_full_entries_do_not_fit() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let catalog = catalog_of(root.path(), 10, 1024);

        let (section, warnings) = prompt_section(&catalog);

        assert!(section.len() <= MAX_LISTING_BYTES, "{}", section.len());
        for definition in catalog.definitions() {
            assert!(
                section.contains(&format!("name=\"{}\"", definition.name)),
                "{} is unreachable: it is not in the listing",
                definition.name
            );
        }
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(!warnings[0].contains("dropped"), "{warnings:?}");
    }

    #[test]
    fn names_survive_a_catalog_whose_descriptions_cannot_fit() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let catalog = catalog_of(root.path(), 100, 1024);

        let (section, warnings) = prompt_section(&catalog);

        assert!(section.len() <= MAX_LISTING_BYTES, "{}", section.len());
        for definition in catalog.definitions() {
            assert!(
                section.contains(&format!("name=\"{}\"", definition.name)),
                "{} is unreachable: it is not in the listing",
                definition.name
            );
        }
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(!warnings[0].contains("dropped"), "{warnings:?}");
    }

    #[test]
    fn a_catalog_too_large_even_for_names_drops_with_one_warning() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let catalog = catalog_of(root.path(), 400, 64);

        let (section, warnings) = prompt_section(&catalog);

        assert!(section.len() <= MAX_LISTING_BYTES, "{}", section.len());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("dropped: "), "{warnings:?}");
    }
}
