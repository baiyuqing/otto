//! `.gitignore` matching for the recursive search tools.
//!
//! `grep` and `find` skip what the repository itself ignores, so a search does
//! not spend its match and byte budget on build output or vendored
//! dependencies. The rules implemented here are git's, because that is the file
//! the workspace already contains and the behavior a user expects:
//!
//! - a pattern is read relative to the directory of the `.gitignore` holding
//!   it, and a deeper file overrides a shallower one;
//! - within one file the last matching pattern decides, so a later `!` pattern
//!   re-includes a path an earlier pattern excluded;
//! - a trailing `/` matches directories only; a leading or interior `/` anchors
//!   the pattern to its own directory, and an unanchored pattern matches at any
//!   depth below it;
//! - `*` and `?` do not cross `/`, `**` does, and `\` escapes the next
//!   character.
//!
//! `.git/info/exclude` and the global `core.excludesFile` are deliberately not
//! read: they live outside the workspace or inside `.git`, which the search
//! tools never enter.
//!
//! Ownership: a [`Gitignore`] owns its parsed patterns; a [`GitignoreStack`]
//! owns one [`Gitignore`] per directory on the current walk path. Concurrency:
//! both are plain values used by one walk. Errors: an unreadable or non-UTF-8
//! `.gitignore` is treated as absent rather than failing the search.

use std::path::Path;

use super::root::Root;

/// The largest `.gitignore` read. A larger file is ignored rather than loaded.
const MAX_GITIGNORE_BYTES: u64 = 1 << 20;

/// What a set of patterns says about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// No pattern matched; a shallower file decides.
    None,
    Ignore,
    /// A `!` pattern matched; the path is kept even if a shallower file
    /// ignores it.
    Include,
}

/// One parsed pattern line.
#[derive(Debug)]
struct Pattern {
    /// The pattern split on `/`, with the anchoring leading slash removed.
    segments: Vec<String>,
    negated: bool,
    directory_only: bool,
    /// Whether the pattern is tied to the directory of its `.gitignore`.
    anchored: bool,
}

/// The patterns of one `.gitignore` file.
#[derive(Debug, Default)]
pub(crate) struct Gitignore {
    patterns: Vec<Pattern>,
}

impl Gitignore {
    /// Parses the contents of one `.gitignore`.
    pub(crate) fn parse(contents: &str) -> Self {
        let mut patterns = Vec::new();
        for line in contents.lines() {
            if let Some(pattern) = parse_pattern(line) {
                patterns.push(pattern);
            }
        }
        Self { patterns }
    }

    /// Reads the `.gitignore` in the root-relative `directory`, returning an
    /// empty set when there is none.
    pub(crate) fn read(root_fs: &Root, directory: &str) -> Self {
        let path = if directory == "." {
            ".gitignore".to_owned()
        } else {
            format!("{directory}/.gitignore")
        };
        let Ok(file) = root_fs.open_file(
            Path::new(&path),
            nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        ) else {
            return Self::default();
        };
        match file.metadata() {
            Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_GITIGNORE_BYTES => {}
            _ => return Self::default(),
        }
        let mut contents = String::new();
        if std::io::Read::read_to_string(&mut { file }, &mut contents).is_err() {
            return Self::default();
        }
        Self::parse(&contents)
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Decides `relative`, a slash path relative to this file's own directory.
    /// The last matching pattern wins, as in git.
    fn decide(&self, relative: &str, is_dir: bool) -> Decision {
        let segments: Vec<&str> = relative.split('/').collect();
        for pattern in self.patterns.iter().rev() {
            if pattern.directory_only && !is_dir {
                continue;
            }
            if pattern.matches(&segments) {
                return if pattern.negated {
                    Decision::Include
                } else {
                    Decision::Ignore
                };
            }
        }
        Decision::None
    }
}

impl Pattern {
    /// Matches the pattern against a path already split on `/`. An unanchored
    /// pattern may start at any segment; an anchored one must start at the
    /// first.
    fn matches(&self, segments: &[&str]) -> bool {
        if self.anchored {
            return match_segments(&self.segments, segments);
        }
        // An unanchored pattern matches the path itself or any of its parents,
        // so that ignoring `target` also ignores `target/debug/app`.
        (0..segments.len()).any(|start| match_segments(&self.segments, &segments[start..]))
    }
}

/// Matches pattern segments against name segments, allowing a trailing
/// remainder so that ignoring a directory ignores everything below it.
fn match_segments(patterns: &[String], names: &[&str]) -> bool {
    let mut memo = vec![None; (patterns.len() + 1) * (names.len() + 1)];
    matches_from(patterns, names, 0, 0, &mut memo)
}

fn matches_from(
    patterns: &[String],
    names: &[&str],
    pattern_index: usize,
    name_index: usize,
    memo: &mut Vec<Option<bool>>,
) -> bool {
    let key = pattern_index * (names.len() + 1) + name_index;
    if let Some(cached) = memo[key] {
        return cached;
    }
    memo[key] = Some(false);

    let matched = if pattern_index == patterns.len() {
        // Every pattern segment matched. Anything left over is below an
        // ignored path and is ignored with it.
        true
    } else if patterns[pattern_index] == "**" {
        matches_from(patterns, names, pattern_index + 1, name_index, memo)
            || (name_index < names.len()
                && matches_from(patterns, names, pattern_index, name_index + 1, memo))
    } else if name_index < names.len() {
        match_segment(&patterns[pattern_index], names[name_index])
            && matches_from(patterns, names, pattern_index + 1, name_index + 1, memo)
    } else {
        false
    };
    memo[key] = Some(matched);
    matched
}

/// Matches one `*`/`?`/`[...]`-style segment against one name segment. Neither
/// wildcard crosses a separator, because the caller has already split on `/`.
fn match_segment(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut memo = vec![None; (pattern.len() + 1) * (name.len() + 1)];
    segment_matches_from(&pattern, &name, 0, 0, &mut memo)
}

fn segment_matches_from(
    pattern: &[char],
    name: &[char],
    mut pattern_index: usize,
    name_index: usize,
    memo: &mut Vec<Option<bool>>,
) -> bool {
    let key = pattern_index * (name.len() + 1) + name_index;
    if let Some(cached) = memo[key] {
        return cached;
    }
    let matched = 'matched: {
        if pattern_index == pattern.len() {
            break 'matched name_index == name.len();
        }
        match pattern[pattern_index] {
            '*' => {
                // Collapse a run of stars; within one segment they are one.
                while pattern_index + 1 < pattern.len() && pattern[pattern_index + 1] == '*' {
                    pattern_index += 1;
                }
                (name_index..=name.len())
                    .any(|next| segment_matches_from(pattern, name, pattern_index + 1, next, memo))
            }
            '?' => {
                name_index < name.len()
                    && segment_matches_from(pattern, name, pattern_index + 1, name_index + 1, memo)
            }
            '[' => {
                let Some(class) = parse_class(pattern, pattern_index) else {
                    break 'matched false;
                };
                name_index < name.len()
                    && (class_contains(&class.members, name[name_index]) != class.negated)
                    && segment_matches_from(pattern, name, class.end, name_index + 1, memo)
            }
            '\\' if pattern_index + 1 < pattern.len() => {
                name_index < name.len()
                    && pattern[pattern_index + 1] == name[name_index]
                    && segment_matches_from(pattern, name, pattern_index + 2, name_index + 1, memo)
            }
            literal => {
                name_index < name.len()
                    && literal == name[name_index]
                    && segment_matches_from(pattern, name, pattern_index + 1, name_index + 1, memo)
            }
        }
    };
    memo[key] = Some(matched);
    matched
}

/// One parsed `[...]` class.
struct CharClass {
    /// The index just past the closing bracket.
    end: usize,
    negated: bool,
    /// The members, each an inclusive range; a single character is a range of
    /// itself.
    members: Vec<(char, char)>,
}

/// Parses a `[...]` class starting at `start`, or `None` when it is unclosed.
fn parse_class(pattern: &[char], start: usize) -> Option<CharClass> {
    let mut index = start + 1;
    let negated = matches!(pattern.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let mut members = Vec::new();
    let mut first = true;
    while index < pattern.len() {
        if pattern[index] == ']' && !first {
            return Some(CharClass {
                end: index + 1,
                negated,
                members,
            });
        }
        first = false;
        let low = if pattern[index] == '\\' && index + 1 < pattern.len() {
            index += 1;
            pattern[index]
        } else {
            pattern[index]
        };
        index += 1;
        if matches!(pattern.get(index), Some('-'))
            && matches!(pattern.get(index + 1), Some(high) if *high != ']')
        {
            index += 1;
            let high = if pattern[index] == '\\' && index + 1 < pattern.len() {
                index += 1;
                pattern[index]
            } else {
                pattern[index]
            };
            index += 1;
            members.push((low, high));
            continue;
        }
        members.push((low, low));
    }
    None
}

fn class_contains(members: &[(char, char)], character: char) -> bool {
    members
        .iter()
        .any(|(low, high)| *low <= character && character <= *high)
}

/// Parses one line, returning `None` for a blank line or a comment.
fn parse_pattern(line: &str) -> Option<Pattern> {
    let line = trim_pattern_end(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, line) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (directory_only, line) = match line.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    if line.is_empty() {
        return None;
    }
    // A slash anywhere but at the end anchors the pattern; a leading one only
    // anchors and is not part of the pattern.
    let anchored = line.trim_end_matches('/').contains('/');
    let line = line.strip_prefix('/').unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    let segments: Vec<String> = line
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.is_empty() {
        return None;
    }
    Some(Pattern {
        segments,
        negated,
        directory_only,
        anchored,
    })
}

/// Removes the trailing whitespace git ignores, keeping a `\`-escaped space.
fn trim_pattern_end(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        // A backslash before the space escapes it, and an even number of
        // backslashes escapes the backslashes instead.
        let backslashes = bytes[..end - 1]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

/// The `.gitignore` files that apply to the directory a walk currently visits,
/// outermost first.
#[derive(Debug, Default)]
pub(crate) struct GitignoreStack {
    /// The directory each set was read from and its patterns. The directory is
    /// the root-relative slash path, or `.` for the search root.
    levels: Vec<(String, Gitignore)>,
}

impl GitignoreStack {
    /// Builds the stack for a search rooted at `root`, reading the `.gitignore`
    /// of `root` and of each of its parents up to the workspace root, so a
    /// scoped search still honors the repository's top-level rules.
    ///
    /// A `root` that those parent rules ignore is one the caller asked for by
    /// name, so the rules above it are dropped and it is searched whole; the
    /// `.gitignore` files inside it still apply.
    pub(crate) fn for_root(root_fs: &Root, root: &str) -> Self {
        let mut stack = Self::from_parents(root_fs, root);
        if stack.is_ignored(root, true) {
            stack.levels.retain(|(owner, _)| owner == root);
        }
        stack
    }

    fn from_parents(root_fs: &Root, root: &str) -> Self {
        let mut levels = Vec::new();
        let mut directory = String::from(".");
        for segment in root
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            let patterns = Gitignore::read(root_fs, &directory);
            if !patterns.is_empty() {
                levels.push((directory.clone(), patterns));
            }
            directory = if directory == "." {
                segment.to_owned()
            } else {
                format!("{directory}/{segment}")
            };
        }
        let patterns = Gitignore::read(root_fs, &directory);
        if !patterns.is_empty() {
            levels.push((directory, patterns));
        }
        Self { levels }
    }

    /// Adds the `.gitignore` of `directory`, which the walk has just entered.
    /// A directory already on the stack, such as the search root that
    /// [`GitignoreStack::for_root`] read, is not read twice.
    pub(crate) fn push(&mut self, root_fs: &Root, directory: &str) {
        if self.levels.iter().any(|(owner, _)| owner == directory) {
            return;
        }
        let patterns = Gitignore::read(root_fs, directory);
        if !patterns.is_empty() {
            self.levels.push((directory.to_owned(), patterns));
        }
    }

    /// Drops every set read from a directory the walk has left, which is every
    /// set whose directory is not `directory` or one of its parents.
    pub(crate) fn truncate_to(&mut self, directory: &str) {
        self.levels
            .retain(|(owner, _)| is_self_or_ancestor(owner, directory));
    }

    /// Reports whether the root-relative `path` is ignored. The deepest file
    /// with an opinion decides, matching git.
    pub(crate) fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        for (owner, patterns) in self.levels.iter().rev() {
            let Some(relative) = relative_to(owner, path) else {
                continue;
            };
            match patterns.decide(relative, is_dir) {
                Decision::Ignore => return true,
                Decision::Include => return false,
                Decision::None => {}
            }
        }
        false
    }
}

/// Reports whether `owner` is `directory` or one of its ancestors.
fn is_self_or_ancestor(owner: &str, directory: &str) -> bool {
    owner == "." || owner == directory || directory.starts_with(&format!("{owner}/"))
}

/// The part of `path` below `owner`, or `None` when `path` is not below it.
fn relative_to<'a>(owner: &str, path: &'a str) -> Option<&'a str> {
    if owner == "." {
        return Some(path);
    }
    path.strip_prefix(owner)?.strip_prefix('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(contents: &str, path: &str, is_dir: bool) -> Decision {
        Gitignore::parse(contents).decide(path, is_dir)
    }

    #[test]
    fn a_plain_pattern_matches_at_any_depth() {
        for (path, want) in [
            ("target", Decision::Ignore),
            ("target/debug", Decision::Ignore),
            ("crates/otto/target/debug/app", Decision::Ignore),
            ("targets", Decision::None),
            ("src/main.rs", Decision::None),
        ] {
            assert_eq!(decide("target\n", path, false), want, "{path}");
        }
    }

    #[test]
    fn a_leading_slash_anchors_the_pattern() {
        assert_eq!(decide("/target\n", "target/x", false), Decision::Ignore);
        assert_eq!(decide("/target\n", "a/target/x", false), Decision::None);
    }

    #[test]
    fn an_interior_slash_anchors_the_pattern() {
        assert_eq!(decide("doc/build\n", "doc/build", false), Decision::Ignore);
        assert_eq!(decide("doc/build\n", "a/doc/build", false), Decision::None);
    }

    #[test]
    fn a_trailing_slash_matches_directories_only() {
        assert_eq!(decide("build/\n", "build", true), Decision::Ignore);
        assert_eq!(decide("build/\n", "build", false), Decision::None);
    }

    #[test]
    fn the_last_matching_pattern_wins() {
        let contents = "*.log\n!keep.log\n";
        assert_eq!(decide(contents, "a.log", false), Decision::Ignore);
        assert_eq!(decide(contents, "keep.log", false), Decision::Include);
    }

    #[test]
    fn wildcards_do_not_cross_a_separator() {
        assert_eq!(decide("a/*.rs\n", "a/b.rs", false), Decision::Ignore);
        assert_eq!(decide("a/*.rs\n", "a/b/c.rs", false), Decision::None);
        assert_eq!(decide("a/**/c.rs\n", "a/b/c.rs", false), Decision::Ignore);
        assert_eq!(decide("a/**/c.rs\n", "a/c.rs", false), Decision::Ignore);
    }

    #[test]
    fn character_classes_and_question_marks_match() {
        assert_eq!(decide("?.rs\n", "a.rs", false), Decision::Ignore);
        assert_eq!(decide("?.rs\n", "ab.rs", false), Decision::None);
        assert_eq!(decide("[ab].rs\n", "b.rs", false), Decision::Ignore);
        assert_eq!(decide("[!ab].rs\n", "c.rs", false), Decision::Ignore);
        assert_eq!(decide("[!ab].rs\n", "a.rs", false), Decision::None);
        assert_eq!(decide("[a-c].rs\n", "b.rs", false), Decision::Ignore);
    }

    #[test]
    fn comments_blank_lines_and_escapes_are_handled() {
        assert_eq!(decide("# target\n", "target", false), Decision::None);
        assert_eq!(decide("\n\n", "target", false), Decision::None);
        assert_eq!(decide("\\#real\n", "#real", false), Decision::Ignore);
        // Trailing spaces are stripped unless escaped.
        assert_eq!(decide("trail   \n", "trail", false), Decision::Ignore);
        assert_eq!(decide("trail\\ \n", "trail ", false), Decision::Ignore);
    }
}
