//! Offline Rex title fixture for both TUI entry points.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use super::pty::{Shared, wait_until};

pub fn configure(command: &mut Command, home: &Path) {
    let rex = home.join("rex");
    std::fs::write(
        &rex,
        r#"#!/bin/sh
[ "$#" -eq 10 ] && [ "$*" = '--autostart=false --timeout=500ms block call -s session:test -b block:test title {}' ] || exit 1
printf '%s\n' '{"title":"previous title"}'
"#,
    )
    .expect("write mock Rex CLI");
    std::fs::set_permissions(&rex, std::fs::Permissions::from_mode(0o700))
        .expect("make mock Rex CLI executable");
    command
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", home.display()),
        )
        .env("REX_SESSION", "session:test")
        .env("REX_BLOCK", "block:test");
}

pub fn assert_transitions(shared: &Shared, expected: &[&str]) {
    wait_until(shared, "Rex title transitions", |shared| {
        let raw = shared.raw.lock().unwrap();
        let mut titles = raw.split(|&byte| byte == b'\x1b').filter_map(|prefix| {
            prefix
                .strip_prefix(b"]2;")
                .and_then(|title| std::str::from_utf8(title).ok())
        });
        titles.by_ref().eq(expected.iter().copied())
    });
}
