//! Lexical path helpers with Go `path/filepath` semantics.
//!
//! The workspace boundary and the search tools compare, split, and rebuild
//! paths exactly the way `internal/tool` does. Rust's `std::path` has no
//! lexical `Clean`, and its `Path::join`/`parent` differ from Go's `Join`/`Dir`
//! on trailing separators, repeated separators, and `..`, so the Go functions
//! are reproduced here on raw bytes.
//!
//! Every function is pure and allocation-only; none of them touch the
//! filesystem, so a lexically clean path says nothing about what exists.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Borrows a path as the raw bytes the Go functions operate on.
pub(crate) fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Wraps raw bytes back into a path without validating UTF-8.
pub(crate) fn path_from(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

/// Reports whether `path` starts at the filesystem root, like `filepath.IsAbs`.
pub(crate) fn is_abs(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
}

/// Reports whether any component of `path` is `..`, like `hasParentTraversal`.
pub(crate) fn has_parent_traversal(path: &[u8]) -> bool {
    path.split(|byte| *byte == b'/').any(|part| part == b"..")
}

/// Returns the shortest lexically equivalent path, like `filepath.Clean`.
pub(crate) fn clean(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let rooted = path[0] == b'/';
    let n = path.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut read = 0usize;
    let mut dotdot = 0usize;
    if rooted {
        out.push(b'/');
        read = 1;
        dotdot = 1;
    }
    while read < n {
        // Go splits an empty element and a lone `.` element into two switch
        // arms with the same body; both drop the element.
        if path[read] == b'/' || (path[read] == b'.' && (read + 1 == n || path[read + 1] == b'/')) {
            read += 1;
        } else if path[read] == b'.'
            && read + 1 < n
            && path[read + 1] == b'.'
            && (read + 2 == n || path[read + 2] == b'/')
        {
            read += 2;
            if out.len() > dotdot {
                // Go's lazybuf rewinds the write cursor to the separator that
                // starts the last element and keeps everything before it.
                let mut write = out.len() - 1;
                while write > dotdot && out[write] != b'/' {
                    write -= 1;
                }
                out.truncate(write);
            } else if !rooted {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while read < n && path[read] != b'/' {
                out.push(path[read]);
                read += 1;
            }
        }
    }
    if out.is_empty() {
        return b".".to_vec();
    }
    out
}

/// Returns all but the last element of `path`, cleaned, like `filepath.Dir`.
pub(crate) fn dir(path: &[u8]) -> Vec<u8> {
    let mut end = path.len();
    while end > 0 && path[end - 1] != b'/' {
        end -= 1;
    }
    clean(&path[..end])
}

/// Returns the last element of `path`, like `filepath.Base`.
pub(crate) fn base(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    let mut start = end;
    while start > 0 && path[start - 1] != b'/' {
        start -= 1;
    }
    path[start..end].to_vec()
}

/// Joins non-empty elements with `/` and cleans the result, like `filepath.Join`.
pub(crate) fn join(elements: &[&[u8]]) -> Vec<u8> {
    let mut joined: Vec<u8> = Vec::new();
    for element in elements {
        if element.is_empty() {
            continue;
        }
        if !joined.is_empty() {
            joined.push(b'/');
        }
        joined.extend_from_slice(element);
    }
    if joined.is_empty() {
        return Vec::new();
    }
    clean(&joined)
}

/// Returns a path relative to `base` that joins back to `target`, like
/// `filepath.Rel`. Fails when the two cannot be related lexically.
pub(crate) fn rel(base_path: &[u8], target: &[u8]) -> std::io::Result<Vec<u8>> {
    let base_clean = clean(base_path);
    let target_clean = clean(target);
    if base_clean == target_clean {
        return Ok(b".".to_vec());
    }
    let base_slice: &[u8] = if base_clean == b"." { b"" } else { &base_clean };
    let target_slice: &[u8] = if target_clean == b"." {
        b""
    } else {
        &target_clean
    };
    if is_abs(base_slice) != is_abs(target_slice) {
        return Err(relation_error(base_path, target));
    }

    let (base_len, target_len) = (base_slice.len(), target_slice.len());
    let (mut b0, mut bi, mut t0, mut ti) = (0usize, 0usize, 0usize, 0usize);
    loop {
        while bi < base_len && base_slice[bi] != b'/' {
            bi += 1;
        }
        while ti < target_len && target_slice[ti] != b'/' {
            ti += 1;
        }
        if target_slice[t0..ti] != base_slice[b0..bi] {
            break;
        }
        if bi < base_len {
            bi += 1;
        }
        if ti < target_len {
            ti += 1;
        }
        b0 = bi;
        t0 = ti;
    }
    if &base_slice[b0..bi] == b".." {
        return Err(relation_error(base_path, target));
    }
    if b0 != base_len {
        let separators = base_slice[b0..base_len]
            .iter()
            .filter(|byte| **byte == b'/')
            .count();
        let mut out: Vec<u8> = b"..".to_vec();
        for _ in 0..separators {
            out.push(b'/');
            out.extend_from_slice(b"..");
        }
        if t0 != target_len {
            out.push(b'/');
            out.extend_from_slice(&target_slice[t0..]);
        }
        return Ok(out);
    }
    Ok(target_slice[t0..].to_vec())
}

fn relation_error(base_path: &[u8], target: &[u8]) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "Rel: can't make {} relative to {}",
            OsStr::from_bytes(target).to_string_lossy(),
            OsStr::from_bytes(base_path).to_string_lossy()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_matches_go_filepath_clean() {
        let cases: &[(&str, &str)] = &[
            ("", "."),
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("a/b/c/", "a/b/c"),
            ("./", "."),
            ("../", ".."),
            ("/abc/", "/abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("//abc", "/abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ];
        for (input, want) in cases {
            assert_eq!(
                String::from_utf8(clean(input.as_bytes())).unwrap(),
                *want,
                "clean({input:?})"
            );
        }
    }

    #[test]
    fn dir_and_base_match_go() {
        let cases: &[(&str, &str, &str)] = &[
            ("", ".", "."),
            (".", ".", "."),
            ("/.", "/", "."),
            ("/", "/", "/"),
            ("/foo", "/", "foo"),
            ("x/", "x", "x"),
            ("abc", ".", "abc"),
            ("abc/def", "abc", "def"),
            ("a/b/c.x", "a/b", "c.x"),
            ("/a/b/c", "/a/b", "c"),
            ("////", "/", "/"),
        ];
        for (input, want_dir, want_base) in cases {
            assert_eq!(
                String::from_utf8(dir(input.as_bytes())).unwrap(),
                *want_dir,
                "dir({input:?})"
            );
            assert_eq!(
                String::from_utf8(base(input.as_bytes())).unwrap(),
                *want_base,
                "base({input:?})"
            );
        }
    }

    #[test]
    fn join_skips_empty_elements_and_cleans() {
        assert_eq!(join(&[b"a", b"b"]), b"a/b");
        assert_eq!(join(&[b"a", b""]), b"a");
        assert_eq!(join(&[b"", b"b"]), b"b");
        assert_eq!(join(&[b"", b""]), b"");
        assert_eq!(join(&[b"/", b"a"]), b"/a");
        assert_eq!(join(&[b"a/", b"b"]), b"a/b");
        assert_eq!(join(&[b"a", b"../b"]), b"b");
    }

    #[test]
    fn rel_matches_go_filepath_rel() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("a/b", "a/b", Some(".")),
            ("a/b/.", "a/b", Some(".")),
            ("a/b", "a/b/.", Some(".")),
            ("./a/b", "a/b", Some(".")),
            ("a/b", "./a/b", Some(".")),
            ("ab/cd", "ab/cde", Some("../cde")),
            ("ab/cd", "ab/c", Some("../c")),
            ("a/b", "a/b/c/d", Some("c/d")),
            ("a/b", "a/b/../c", Some("../c")),
            ("a/b/../c", "a/b", Some("../b")),
            ("a/b/c", "a/c/d", Some("../../c/d")),
            ("a/b", "c/d", Some("../../c/d")),
            ("a/b/c/d", "a/b", Some("../..")),
            ("a/b/c/d", "a/b/", Some("../..")),
            ("a", "..", Some("../..")),
            ("a", "../b", Some("../../b")),
            ("/a/b", "/a/b", Some(".")),
            ("/a/b/c", "/a/b", Some("..")),
            ("/a/b", "/a/b/c", Some("c")),
            ("..", "a", None),
            ("/a", "b", None),
            ("a", "/b", None),
        ];
        for (base_path, target, want) in cases {
            let got = rel(base_path.as_bytes(), target.as_bytes());
            match want {
                Some(expected) => assert_eq!(
                    String::from_utf8(got.expect("rel should succeed")).unwrap(),
                    *expected,
                    "rel({base_path:?}, {target:?})"
                ),
                None => assert!(got.is_err(), "rel({base_path:?}, {target:?}) should fail"),
            }
        }
    }

    #[test]
    fn parent_traversal_looks_at_whole_components() {
        assert!(has_parent_traversal(b".."));
        assert!(has_parent_traversal(b"a/../b"));
        assert!(has_parent_traversal(b"../a"));
        assert!(!has_parent_traversal(b"..a"));
        assert!(!has_parent_traversal(b"a/..b/c"));
        assert!(!has_parent_traversal(b"a/b"));
    }
}
