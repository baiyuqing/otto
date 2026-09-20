//! Lexical path helpers shared by `skills`, `agents`, `server`, and `memory`
//! resolution, on the forward-slash paths Otto resolves against (Otto runs on
//! macOS only).
//!
//! These duplicate a subset of the byte-exact algorithm in `otto::tool::gopath`
//! rather than reusing it: this crate cannot depend on the native `otto` crate,
//! and only `Clean`/`Join`'s ordinary behavior is needed here (no
//! `..`-traversal security checks, which stay owned by the workspace-boundary
//! tool code).

/// Returns the shortest lexically equivalent path, like `filepath.Clean`.
pub(crate) fn clean(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return ".".to_string();
    }
    let rooted = bytes[0] == b'/';
    let n = bytes.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut read = 0usize;
    let mut dotdot = 0usize;
    if rooted {
        out.push(b'/');
        read = 1;
        dotdot = 1;
    }
    while read < n {
        if bytes[read] == b'/'
            || (bytes[read] == b'.' && (read + 1 == n || bytes[read + 1] == b'/'))
        {
            read += 1;
        } else if bytes[read] == b'.'
            && read + 1 < n
            && bytes[read + 1] == b'.'
            && (read + 2 == n || bytes[read + 2] == b'/')
        {
            read += 2;
            if out.len() > dotdot {
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
            while read < n && bytes[read] != b'/' {
                out.push(bytes[read]);
                read += 1;
            }
        }
    }
    if out.is_empty() {
        return ".".to_string();
    }
    String::from_utf8(out).expect("clean() only rearranges bytes of a valid UTF-8 input")
}

/// Joins `base` and `rel` with a separator and cleans the result, like
/// `filepath.Join` for two elements.
pub(crate) fn join(base: &str, rel: &str) -> String {
    if base.is_empty() {
        return clean(rel);
    }
    if rel.is_empty() {
        return clean(base);
    }
    clean(&format!("{base}/{rel}"))
}

/// Reports whether `path` starts at the filesystem root, like `filepath.IsAbs`
/// on macOS/Linux.
pub(crate) fn is_abs(path: &str) -> bool {
    path.starts_with('/')
}

/// Resolves `env`'s home directory: the native layer is responsible for falling
/// back to the real process `HOME` before calling in, so this only reads the
/// injected value.
pub(crate) fn home_from_env(env: &std::collections::HashMap<String, String>) -> &str {
    env.get("HOME").map(String::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn cleans_dot_dot_and_repeated_separators() {
        assert_eq!(clean("a/b/../c"), "a/c");
        assert_eq!(clean("a//b"), "a/b");
        assert_eq!(clean("./a"), "a");
        assert_eq!(clean(""), ".");
        assert_eq!(clean("/abs/skills-c"), "/abs/skills-c");
        assert_eq!(clean("../a"), "../a");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn joins_like_filepath_join() {
        assert_eq!(
            join("/work", "relative/skills-b"),
            "/work/relative/skills-b"
        );
        assert_eq!(join("/home", "skills-a"), "/home/skills-a");
    }
}
