//! `GET /v1/fs/dirs`: lists subdirectories for the web UI's folder picker.
//!
//! A browser cannot report the absolute path of a folder the user picks, so
//! the UI browses the server's filesystem through this route instead. Listing
//! is confined to [`Factory::browse_roots`](super::Factory::browse_roots) and
//! their descendants, checked after canonicalization so a symlink cannot lead
//! outside them. Only directory names are returned, never file names.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::{Server, error_response, json_response};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirsQuery {
    path: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Dir {
    name: String,
    path: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Listing {
    /// The canonical directory listed.
    path: String,
    /// Its parent, or `None` when the parent is outside every browse root.
    parent: Option<String>,
    /// The browse roots, so the UI can offer them as starting points.
    roots: Vec<String>,
    /// Subdirectories sorted by name, case-insensitively. Names starting
    /// with `.` are left out.
    dirs: Vec<Dir>,
}

#[derive(Debug, PartialEq)]
pub enum ListError {
    /// Not an absolute path to an existing directory.
    Invalid(String),
    /// A real directory outside every browse root.
    OutsideRoots(String),
    /// Reading the directory failed.
    Unreadable(String),
}

pub async fn dirs(State(server): State<Arc<Server>>, Query(query): Query<DirsQuery>) -> Response {
    let roots = server.factory.browse_roots();
    if roots.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            "BROWSE_UNAVAILABLE",
            "directory browsing is not available",
        );
    }
    let listed = tokio::task::spawn_blocking(move || list(query.path.as_deref(), &roots)).await;
    match listed {
        Ok(Ok(listing)) => json_response(StatusCode::OK, &listing),
        Ok(Err(ListError::Invalid(message))) => {
            error_response(StatusCode::BAD_REQUEST, "INVALID_PATH", &message)
        }
        Ok(Err(ListError::OutsideRoots(message))) => {
            error_response(StatusCode::FORBIDDEN, "PATH_NOT_ALLOWED", &message)
        }
        Ok(Err(ListError::Unreadable(message))) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", &message)
        }
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &error.to_string(),
        ),
    }
}

/// Lists `requested` (the first root when `None`). `roots` must be canonical.
pub fn list(requested: Option<&str>, roots: &[PathBuf]) -> Result<Listing, ListError> {
    let requested = match requested {
        Some(path) => PathBuf::from(path),
        None => roots
            .first()
            .cloned()
            .ok_or_else(|| ListError::Invalid("no browse root".to_string()))?,
    };
    let shown = requested.display().to_string();
    if !requested.is_absolute() {
        return Err(ListError::Invalid(format!("{shown}: not an absolute path")));
    }
    let dir = crate::cli::sandbox_runtime::canonical_directory(&requested)
        .map_err(|_| ListError::Invalid(format!("{shown}: not an existing directory")))?;
    let within = |path: &Path| roots.iter().any(|root| path.starts_with(root));
    if !within(&dir) {
        return Err(ListError::OutsideRoots(format!(
            "{shown}: outside the browsable directories"
        )));
    }
    let entries = std::fs::read_dir(&dir)
        .map_err(|error| ListError::Unreadable(format!("{shown}: {error}")))?;
    let mut dirs: Vec<Dir> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            // `metadata` follows symlinks, so a link to a directory is listed;
            // where it leads is checked when the UI opens it.
            let is_dir = std::fs::metadata(entry.path()).ok()?.is_dir();
            (is_dir && !name.starts_with('.')).then(|| Dir {
                path: entry.path().to_string_lossy().into_owned(),
                name,
            })
        })
        .collect();
    dirs.sort_by_cached_key(|dir| dir.name.to_lowercase());
    Ok(Listing {
        parent: dir
            .parent()
            .filter(|parent| within(parent))
            .map(|parent| parent.to_string_lossy().into_owned()),
        path: dir.to_string_lossy().into_owned(),
        roots: roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect(),
        dirs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).expect("canonical")
    }

    fn names(listing: &Listing) -> Vec<&str> {
        listing.dirs.iter().map(|dir| dir.name.as_str()).collect()
    }

    #[test]
    fn no_path_lists_the_first_root_with_directories_only_sorted_and_no_dot_names() {
        let home = tempfile::tempdir().expect("home");
        let root = canonical(home.path());
        for name in ["beta", "Alpha", ".hidden", "gamma"] {
            std::fs::create_dir(root.join(name)).expect("mkdir");
        }
        std::fs::write(root.join("file.txt"), "x").expect("file");

        let listing = list(None, std::slice::from_ref(&root)).expect("listing");

        assert_eq!(listing.path, root.to_string_lossy());
        assert_eq!(listing.parent, None);
        assert_eq!(listing.roots, vec![root.to_string_lossy().into_owned()]);
        assert_eq!(names(&listing), ["Alpha", "beta", "gamma"]);
        assert_eq!(listing.dirs[0].path, root.join("Alpha").to_string_lossy());
    }

    #[test]
    fn a_descendant_reports_its_parent() {
        let home = tempfile::tempdir().expect("home");
        let root = canonical(home.path());
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir");

        let listing = list(
            Some(&root.join("a").to_string_lossy()),
            std::slice::from_ref(&root),
        )
        .expect("listing");

        assert_eq!(listing.parent.as_deref(), Some(&*root.to_string_lossy()));
        assert_eq!(names(&listing), ["b"]);
    }

    #[test]
    fn a_directory_outside_every_root_is_refused() {
        let home = tempfile::tempdir().expect("home");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let root = canonical(home.path());

        let got = list(
            Some(&elsewhere.path().to_string_lossy()),
            std::slice::from_ref(&root),
        );

        assert!(matches!(got, Err(ListError::OutsideRoots(_))), "{got:?}");
    }

    #[test]
    fn a_symlink_leading_outside_every_root_is_refused() {
        let home = tempfile::tempdir().expect("home");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let root = canonical(home.path());
        std::os::unix::fs::symlink(elsewhere.path(), root.join("link")).expect("symlink");

        let got = list(
            Some(&root.join("link").to_string_lossy()),
            std::slice::from_ref(&root),
        );

        assert!(matches!(got, Err(ListError::OutsideRoots(_))), "{got:?}");
    }

    #[test]
    fn a_relative_path_or_a_file_is_invalid() {
        let home = tempfile::tempdir().expect("home");
        let root = canonical(home.path());
        std::fs::write(root.join("file.txt"), "x").expect("file");

        let relative = list(Some("relative"), std::slice::from_ref(&root));
        let file = list(
            Some(&root.join("file.txt").to_string_lossy()),
            std::slice::from_ref(&root),
        );

        assert!(
            matches!(relative, Err(ListError::Invalid(_))),
            "{relative:?}"
        );
        assert!(matches!(file, Err(ListError::Invalid(_))), "{file:?}");
    }

    #[test]
    fn a_second_root_and_its_descendants_are_listable() {
        let home = tempfile::tempdir().expect("home");
        let extra = tempfile::tempdir().expect("extra");
        let roots = vec![canonical(home.path()), canonical(extra.path())];
        std::fs::create_dir(roots[1].join("project")).expect("mkdir");

        let listing = list(Some(&roots[1].to_string_lossy()), &roots).expect("listing");

        assert_eq!(listing.parent, None);
        assert_eq!(names(&listing), ["project"]);
    }
}
