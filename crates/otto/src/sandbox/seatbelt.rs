//! The Seatbelt driver.
//!
//! Port of `internal/sandbox/seatbelt`. The driver confines each child with
//! `/usr/bin/sandbox-exec` and a profile generated from the session's
//! workspace, private state directories and reviewed read roots.

/// The immutable profile template, included byte for byte from the Go
/// package so the two implementations can never drift.
///
/// The file is the single source of truth for every static rule; the four
/// `@@OTTO_PROFILE_*@@` markers are the only places generated text appears.
pub(crate) const TEMPLATE: &str =
    include_str!("../../../../internal/sandbox/seatbelt/profile_v1.sb");

pub mod driver;
pub(crate) mod profile;
pub(crate) mod selftest;
pub(crate) mod state;

pub use driver::{Options, SeatbeltDriver};

/// The identifier this driver reports, matching the Go `seatbelt.ID`.
pub const ID: &str = "seatbelt";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_included_template_matches_the_go_package_file_byte_for_byte() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../internal/sandbox/seatbelt/profile_v1.sb");
        let on_disk = std::fs::read(&path).expect("read profile_v1.sb");
        assert_eq!(TEMPLATE.as_bytes(), on_disk.as_slice());
    }
}
