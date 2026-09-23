//! Shared search helpers.
//!
//! The glob matcher reproduces Go's `path.Match` per segment plus recursive
//! `**` segments, because the tool contract advertises exactly that syntax. The
//! `.git` rules are a security boundary: a search root inside the repository
//! metadata directory, reached directly or through a symlink alias, returns
//! nothing rather than the contents of the object store.

use std::io;
use std::path::Path;

use super::gitignore::GitignoreStack;
use super::gopath::{base, bytes, clean, is_abs, path_from, rel};
use super::root::Root;
use super::workspace::Workspace;

/// The text Go's `path.ErrBadPattern` carries.
const BAD_PATTERN: &str = "syntax error in pattern";

/// Reports whether `name` matches `pattern`, which may contain recursive `**`
/// segments. The tools split validation from matching so they can validate the
/// pattern once per call instead of once per candidate.
#[cfg(test)]
pub(crate) fn match_recursive_glob(pattern: &str, name: &str) -> Result<bool, String> {
    let segments = validated_glob_segments(pattern)?;
    Ok(match_glob_segments(&segments, name))
}

/// Matches pre-split pattern segments against a slash-separated name. The memo
/// table keeps the `**` backtracking linear in the product of the two lengths.
pub(crate) fn match_glob_segments(segments: &[String], name: &str) -> bool {
    let name = name.strip_prefix("./").unwrap_or(name);
    let name_segments: Vec<&str> = name.split('/').collect();
    let mut memo = vec![None; (segments.len() + 1) * (name_segments.len() + 1)];
    matches_from(segments, &name_segments, 0, 0, &mut memo)
}

fn matches_from(
    segments: &[String],
    name_segments: &[&str],
    pattern_index: usize,
    name_index: usize,
    memo: &mut Vec<Option<bool>>,
) -> bool {
    let key = pattern_index * (name_segments.len() + 1) + name_index;
    if let Some(cached) = memo[key] {
        return cached;
    }
    // The memo is seeded before recursing, so a cycle resolves to false.
    memo[key] = Some(false);

    let matched = if pattern_index == segments.len() {
        name_index == name_segments.len()
    } else if segments[pattern_index] == "**" {
        matches_from(segments, name_segments, pattern_index + 1, name_index, memo)
            || (name_index < name_segments.len()
                && matches_from(segments, name_segments, pattern_index, name_index + 1, memo))
    } else if name_index < name_segments.len() {
        go_match(&segments[pattern_index], name_segments[name_index]).unwrap_or(false)
            && matches_from(
                segments,
                name_segments,
                pattern_index + 1,
                name_index + 1,
                memo,
            )
    } else {
        false
    };
    memo[key] = Some(matched);
    matched
}

/// Splits and validates a glob pattern. The error texts are the ones the model
/// sees.
pub(crate) fn validated_glob_segments(pattern: &str) -> Result<Vec<String>, String> {
    if pattern.is_empty() {
        return Err("pattern must not be empty".to_owned());
    }
    if pattern.starts_with('/') {
        return Err("pattern must be relative".to_owned());
    }
    let segments: Vec<String> = pattern.split('/').map(str::to_owned).collect();
    for segment in &segments {
        if segment.is_empty() || segment == ".." {
            return Err(format!("invalid glob pattern {pattern:?}"));
        }
        if segment == "**" {
            continue;
        }
        if let Err(error) = go_match(segment, "") {
            return Err(format!("invalid glob pattern {pattern:?}: {error}"));
        }
    }
    Ok(segments)
}

/// `/` is the separator, so a `*` never crosses a segment boundary even when a
/// caller passes a full path.
fn go_match(pattern: &str, name: &str) -> Result<bool, &'static str> {
    let mut pattern = pattern;
    let mut name = name;
    while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            // A trailing `*` matches the rest of the name unless it crosses a
            // separator.
            return Ok(!name.contains('/'));
        }
        match match_chunk(chunk, name) {
            Ok(Some(remainder)) if remainder.is_empty() || !pattern.is_empty() => {
                name = remainder;
                continue;
            }
            Err(error) => return Err(error),
            _ => {}
        }
        if star {
            let mut advanced = false;
            let limit = name.find('/').unwrap_or(name.len());
            for index in 0..limit {
                if !name.is_char_boundary(index + 1) {
                    continue;
                }
                match match_chunk(chunk, &name[index + 1..]) {
                    Ok(Some(remainder)) => {
                        if pattern.is_empty() && !remainder.is_empty() {
                            continue;
                        }
                        name = remainder;
                        advanced = true;
                        break;
                    }
                    Err(error) => return Err(error),
                    Ok(None) => {}
                }
            }
            if advanced {
                continue;
            }
        }
        // The match failed; keep scanning so a malformed remainder is still
        // reported as a pattern error rather than a plain mismatch.
        while !pattern.is_empty() {
            let (_, chunk, rest) = scan_chunk(pattern);
            pattern = rest;
            match_chunk(chunk, "")?;
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

/// Splits off the leading stars and the following literal run.
fn scan_chunk(pattern: &str) -> (bool, &str, &str) {
    let mut star = false;
    let mut pattern = pattern;
    while let Some(rest) = pattern.strip_prefix('*') {
        pattern = rest;
        star = true;
    }
    let raw = pattern.as_bytes();
    let mut in_range = false;
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'\\' => {
                if index + 1 < raw.len() {
                    index += 1;
                }
            }
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => break,
            _ => {}
        }
        index += 1;
    }
    let index = index.min(raw.len());
    (star, &pattern[..index], &pattern[index..])
}

/// Matches one star-free chunk against a prefix of `name`, returning the
/// unmatched remainder.
fn match_chunk<'a>(chunk: &str, name: &'a str) -> Result<Option<&'a str>, &'static str> {
    let mut failed = false;
    let mut chunk = chunk;
    let mut name = name;
    while !chunk.is_empty() {
        if !failed && name.is_empty() {
            failed = true;
        }
        if let Some(rest) = chunk.strip_prefix('[') {
            let mut character = '\0';
            if !failed {
                let mut characters = name.chars();
                character = characters.next().expect("name is not empty");
                name = characters.as_str();
            }
            chunk = rest;
            let negated = match chunk.strip_prefix('^') {
                Some(rest) => {
                    chunk = rest;
                    true
                }
                None => false,
            };
            let mut matched = false;
            let mut ranges = 0;
            loop {
                if ranges > 0 && chunk.starts_with(']') {
                    chunk = &chunk[1..];
                    break;
                }
                let (low, rest) = get_escaped(chunk)?;
                chunk = rest;
                let mut high = low;
                if chunk.starts_with('-') {
                    let (bound, rest) = get_escaped(&chunk[1..])?;
                    high = bound;
                    chunk = rest;
                }
                if low <= character && character <= high {
                    matched = true;
                }
                ranges += 1;
            }
            if matched == negated {
                failed = true;
            }
            continue;
        }
        if let Some(rest) = chunk.strip_prefix('?') {
            if !failed {
                let mut characters = name.chars();
                let character = characters.next().expect("name is not empty");
                if character == '/' {
                    failed = true;
                }
                name = characters.as_str();
            }
            chunk = rest;
            continue;
        }
        if chunk.starts_with('\\') {
            chunk = &chunk[1..];
            if chunk.is_empty() {
                return Err(BAD_PATTERN);
            }
        }
        let expected = chunk.as_bytes()[0];
        if !failed {
            if expected != name.as_bytes()[0] {
                failed = true;
            }
            name = &name[1..];
        }
        chunk = &chunk[1..];
    }
    Ok(if failed { None } else { Some(name) })
}

/// Reads one possibly escaped character of a character class.
fn get_escaped(chunk: &str) -> Result<(char, &str), &'static str> {
    if chunk.is_empty() || chunk.starts_with('-') || chunk.starts_with(']') {
        return Err(BAD_PATTERN);
    }
    let chunk = match chunk.strip_prefix('\\') {
        Some("") => return Err(BAD_PATTERN),
        Some(rest) => rest,
        None => chunk,
    };
    let character = chunk.chars().next().expect("chunk is not empty");
    let rest = &chunk[character.len_utf8()..];
    if rest.is_empty() {
        return Err(BAD_PATTERN);
    }
    Ok((character, rest))
}

/// The name a matched file is tested under, relative to the search root.
pub(crate) fn search_relative_path(root: &str, file_path: &str) -> io::Result<String> {
    let relative = rel(root.as_bytes(), file_path.as_bytes())?;
    if relative == b"." {
        return Ok(String::from_utf8_lossy(&base(file_path.as_bytes())).into_owned());
    }
    Ok(String::from_utf8_lossy(&relative).into_owned())
}

/// Validates an optional result limit.
pub(crate) fn resolve_search_limit(
    value: Option<i64>,
    default_value: usize,
    maximum: usize,
) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(default_value);
    };
    if value < 1 {
        return Err("limit must be >= 1".to_owned());
    }
    if value > maximum as i64 {
        return Err(format!("limit must be <= {maximum}"));
    }
    Ok(value as usize)
}

/// Reports whether a search root lies in repository metadata, either lexically
/// or through a `.git` symlink alias that still resolves inside the workspace.
pub(crate) fn search_root_inside_git(
    workspace: &Workspace,
    requested_path: &str,
    resolved_root: &str,
) -> io::Result<bool> {
    if path_has_git_segment(resolved_root) {
        return Ok(true);
    }
    requested_git_alias_inside_workspace(workspace, requested_path)
}

/// Walks the requested path element by element and reports whether any `.git`
/// element resolves to a directory inside the workspace. The workspace root
/// itself may be named `.git`, which is not an alias.
fn requested_git_alias_inside_workspace(
    workspace: &Workspace,
    requested_path: &str,
) -> io::Result<bool> {
    let candidate = if is_abs(requested_path.as_bytes()) {
        requested_path.as_bytes().to_vec()
    } else {
        bytes(&workspace.candidate_path(Path::new(requested_path))).to_vec()
    };
    let absolute = is_abs(&candidate);
    let remainder: &[u8] = candidate
        .strip_prefix(b"/".as_slice())
        .unwrap_or(&candidate);

    let mut prefix = if absolute { vec![b'/'] } else { Vec::new() };
    for segment in remainder.split(|byte| *byte == b'/') {
        if segment.is_empty() {
            continue;
        }
        prefix = super::gopath::join(&[&prefix, segment]);
        if segment != b".git" {
            continue;
        }
        let prefix_path = path_from(prefix.clone());
        let resolved = std::fs::canonicalize(&prefix_path)?;
        let clean_prefix = path_from(clean(&prefix));
        if resolved == workspace.root()
            && (clean_prefix == workspace.root() || clean_prefix == workspace.lexical_root())
        {
            continue;
        }
        if workspace.ensure_inside(&resolved).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reports whether any element of a slash path is `.git`.
pub(crate) fn path_has_git_segment(relative: &str) -> bool {
    relative.split('/').any(|segment| segment == ".git")
}

/// One entry produced by [`walk_dir_ignoring`].
pub(crate) struct WalkEntry {
    /// The slash path relative to the workspace root handle.
    pub(crate) path: String,
    /// The final element of [`WalkEntry::path`].
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
    pub(crate) is_regular: bool,
}

/// What the visitor wants the walk to do next.
pub(crate) enum WalkAction {
    Continue,
    SkipDir,
    Stop,
}

/// Walks `root` depth first in lexical order through the workspace root handle,
/// calling `visit` for the root itself and then every entry. Symbolic links are
/// reported, never followed, so a link out of the workspace is skipped by the
/// caller rather than traversed.
///
/// When `ignore` is set, an entry the repository's `.gitignore` files ignore is
/// not reported, and an ignored directory is not descended into. The search
/// root itself is always reported, so pointing a tool at an ignored directory
/// still searches it.
///
/// Returns `Ok(true)` when the visitor stopped the walk.
pub(crate) fn walk_dir_ignoring(
    root_fs: &Root,
    root: &str,
    ignore: Option<&mut GitignoreStack>,
    visit: &mut dyn FnMut(&WalkEntry) -> io::Result<WalkAction>,
) -> io::Result<bool> {
    let stat = root_fs.stat(Path::new(root))?;
    let entry = WalkEntry {
        path: root.to_owned(),
        name: String::from_utf8_lossy(&base(root.as_bytes())).into_owned(),
        is_dir: super::root::is_dir(&stat),
        is_symlink: false,
        is_regular: super::root::is_regular(&stat),
    };
    let is_dir = entry.is_dir;
    match visit(&entry)? {
        WalkAction::Stop => return Ok(true),
        WalkAction::SkipDir => return Ok(false),
        WalkAction::Continue => {}
    }
    if !is_dir {
        return Ok(false);
    }
    match ignore {
        Some(stack) => walk_children_ignoring(root_fs, root, stack, visit),
        None => walk_children(root_fs, root, visit),
    }
}

/// [`walk_children`] that consults, and extends, the `.gitignore` stack.
fn walk_children_ignoring(
    root_fs: &Root,
    directory: &str,
    stack: &mut GitignoreStack,
    visit: &mut dyn FnMut(&WalkEntry) -> io::Result<WalkAction>,
) -> io::Result<bool> {
    stack.truncate_to(directory);
    stack.push(root_fs, directory);
    for child in root_fs.read_dir(Path::new(directory))? {
        let name = child.name.to_string_lossy().into_owned();
        let path = if directory == "." {
            name.clone()
        } else {
            format!("{directory}/{name}")
        };
        if stack.is_ignored(&path, child.is_dir) {
            continue;
        }
        let entry = WalkEntry {
            path,
            name,
            is_dir: child.is_dir,
            is_symlink: child.is_symlink,
            is_regular: child.is_regular,
        };
        let descend = entry.is_dir;
        let path = entry.path.clone();
        match visit(&entry)? {
            WalkAction::Stop => return Ok(true),
            WalkAction::SkipDir => continue,
            WalkAction::Continue => {}
        }
        if descend && walk_children_ignoring(root_fs, &path, stack, visit)? {
            return Ok(true);
        }
        // A sibling subtree may have pushed its own rules; drop them again.
        stack.truncate_to(directory);
    }
    Ok(false)
}

/// [`walk_dir_ignoring`] without `.gitignore` filtering.
fn walk_children(
    root_fs: &Root,
    directory: &str,
    visit: &mut dyn FnMut(&WalkEntry) -> io::Result<WalkAction>,
) -> io::Result<bool> {
    for child in root_fs.read_dir(Path::new(directory))? {
        let name = child.name.to_string_lossy().into_owned();
        let path = if directory == "." {
            name.clone()
        } else {
            format!("{directory}/{name}")
        };
        let entry = WalkEntry {
            path,
            name,
            is_dir: child.is_dir,
            is_symlink: child.is_symlink,
            is_regular: child.is_regular,
        };
        let descend = entry.is_dir;
        let path = entry.path.clone();
        match visit(&entry)? {
            WalkAction::Stop => return Ok(true),
            WalkAction::SkipDir => continue,
            WalkAction::Continue => {}
        }
        if descend && walk_children(root_fs, &path, visit)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_globs_match_double_star_and_standard_segments() {
        for (pattern, name, want) in [
            ("**/*.go", "main.go", true),
            ("**/*.go", "internal/tool/read.go", true),
            ("src/**/test*.go", "src/test_one.go", true),
            ("src/**/test*.go", "src/a/b/test_two.go", true),
            ("src/**/test*.go", "other/test.go", false),
            ("*.go", "main.go", true),
            ("*.go", "cmd/main.go", false),
            ("file[0-9].txt", "file7.txt", true),
        ] {
            let got = match_recursive_glob(pattern, name).expect("the pattern is valid");
            assert_eq!(got, want, "match({pattern:?}, {name:?})");
        }
    }

    #[test]
    fn invalid_and_escaping_globs_are_rejected() {
        for pattern in ["", "/absolute/**", "../**", "src/[broken"] {
            assert!(
                match_recursive_glob(pattern, "src/main.go").is_err(),
                "match({pattern:?}) should be rejected"
            );
        }
    }

    #[test]
    fn search_limits_follow_the_documented_bounds() {
        assert_eq!(resolve_search_limit(None, 100, 1000).unwrap(), 100);
        assert_eq!(resolve_search_limit(Some(7), 100, 1000).unwrap(), 7);
        assert_eq!(
            resolve_search_limit(Some(0), 100, 1000).unwrap_err(),
            "limit must be >= 1"
        );
        assert_eq!(
            resolve_search_limit(Some(-1), 100, 1000).unwrap_err(),
            "limit must be >= 1"
        );
        assert_eq!(
            resolve_search_limit(Some(1001), 100, 1000).unwrap_err(),
            "limit must be <= 1000"
        );
    }
}

/// Joint `find` + `grep` coverage for the shared `.git` exclusion rules. The
/// two tools route every path through [`search_root_inside_git`] and
/// [`path_has_git_segment`], so the tests exercise them together.
#[cfg(test)]
mod git_alias_tests {
    use crate::tool::find::FindTool;
    use crate::tool::grep::GrepTool;
    use crate::tool::testutil::{MAX_OUTPUT_BYTES, run, workspace, write_search_file};
    use std::path::Path;

    /// Runs both tools against `search_path` and asserts both produce no output.
    async fn both_are_empty(root: &Path, search_path: &str) {
        let workspace = workspace(root);
        let quoted = serde_json::to_string(search_path).unwrap();
        let found = run(
            &FindTool::new(&workspace, MAX_OUTPUT_BYTES),
            &format!(r#"{{"pattern":"**","path":{quoted}}}"#),
        )
        .await;
        let grepped = run(
            &GrepTool::new(&workspace, MAX_OUTPUT_BYTES),
            &format!(r#"{{"pattern":"match","path":{quoted}}}"#),
        )
        .await;
        assert!(
            !found.is_error && found.content.is_empty(),
            "find({search_path}) = {found:?}"
        );
        assert!(
            !grepped.is_error && grepped.content.is_empty(),
            "grep({search_path}) = {grepped:?}"
        );
    }

    #[tokio::test]
    async fn a_git_metadata_file_and_direct_git_aliases_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), ".git", "match metadata\n");
        write_search_file(root.path(), "normal.txt", "match public\n");
        let workspace = workspace(root.path());

        let found = run(
            &FindTool::new(&workspace, MAX_OUTPUT_BYTES),
            r#"{"pattern":"**"}"#,
        )
        .await;
        assert!(
            !found.is_error && found.content == "normal.txt\n",
            "{found:?}"
        );
        let grepped = run(
            &GrepTool::new(&workspace, MAX_OUTPUT_BYTES),
            r#"{"pattern":"match"}"#,
        )
        .await;
        assert!(
            !grepped.is_error && grepped.content == "normal.txt:1:match public\n",
            "{grepped:?}"
        );

        std::fs::remove_file(root.path().join(".git")).unwrap();
        write_search_file(root.path(), "metadata/config", "match alias\n");
        std::os::unix::fs::symlink(root.path().join("metadata"), root.path().join(".git")).unwrap();
        for search_path in [".git", ".git/config"] {
            both_are_empty(root.path(), search_path).await;
        }
    }

    #[tokio::test]
    async fn an_absolute_git_alias_through_a_lexical_workspace_symlink_is_skipped() {
        let parent = tempfile::tempdir().unwrap();
        let real_root = parent.path().join("real");
        std::fs::create_dir_all(&real_root).unwrap();
        write_search_file(&real_root, "metadata/config", "match alias\n");
        std::os::unix::fs::symlink(real_root.join("metadata"), real_root.join(".git")).unwrap();
        let alias_root = parent.path().join("workspace-link");
        std::os::unix::fs::symlink(&real_root, &alias_root).unwrap();
        let sibling_alias = parent.path().join("other-link");
        std::os::unix::fs::symlink(&real_root, &sibling_alias).unwrap();

        let relative_sibling = pathdiff(&real_root, &sibling_alias.join(".git"));
        let paths = [
            alias_root.join(".git").to_string_lossy().into_owned(),
            real_root.join(".git").to_string_lossy().into_owned(),
            sibling_alias.join(".git").to_string_lossy().into_owned(),
            relative_sibling,
            ".git/../metadata".to_string(),
            format!("{}/.git/../metadata", alias_root.display()),
            format!("{}/.git/../metadata", sibling_alias.display()),
        ];
        for search_path in paths {
            both_are_empty(&alias_root, &search_path).await;
        }
    }

    /// The same lexical `Rel` the tools use, so the test can state the sibling
    /// path (`../other-link/.git`) directly.
    fn pathdiff(base: &Path, target: &Path) -> String {
        use std::os::unix::ffi::OsStrExt;
        let relative =
            crate::tool::gopath::rel(base.as_os_str().as_bytes(), target.as_os_str().as_bytes())
                .expect("the paths share a root");
        String::from_utf8(relative).expect("the temporary paths are UTF-8")
    }

    #[tokio::test]
    async fn a_workspace_root_named_git_is_searchable() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join(".git");
        write_search_file(&root, "normal.txt", "match public\n");
        let workspace = workspace(&root);
        let found = run(
            &FindTool::new(&workspace, MAX_OUTPUT_BYTES),
            r#"{"pattern":"**"}"#,
        )
        .await;
        let grepped = run(
            &GrepTool::new(&workspace, MAX_OUTPUT_BYTES),
            r#"{"pattern":"match"}"#,
        )
        .await;
        assert!(
            !found.is_error && found.content == "normal.txt\n",
            "{found:?}"
        );
        assert!(
            !grepped.is_error && grepped.content == "normal.txt:1:match public\n",
            "{grepped:?}"
        );
    }

    #[tokio::test]
    async fn paths_are_normalized_and_git_aliases_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        write_search_file(root.path(), "dir/file.txt", "match\n");
        write_search_file(root.path(), ".git/config", "match secret\n");
        std::os::unix::fs::symlink(".git", root.path().join("alias")).unwrap();
        let workspace = workspace(root.path());
        let find = FindTool::new(&workspace, MAX_OUTPUT_BYTES);
        let grep = GrepTool::new(&workspace, MAX_OUTPUT_BYTES);

        let found = run(&find, r#"{"pattern":"**","path":"./dir/"}"#).await;
        assert!(!found.is_error, "find ./dir/: {found:?}");
        let grepped = run(&grep, r#"{"pattern":"match","path":"./dir/"}"#).await;
        assert!(!grepped.is_error, "grep ./dir/: {grepped:?}");

        both_are_empty(root.path(), "alias").await;
    }
}
