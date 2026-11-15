//! The file-access boundary of tools and the sandbox: the confined
//! `Workspace` (pinned root, no-follow path resolution) and the read-only
//! guard for `.git`. Workspace identity and admission are in
//! `otto_session::workspace`.

pub mod confined;
pub(crate) mod git_read_only;
