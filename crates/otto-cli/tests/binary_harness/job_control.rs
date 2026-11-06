//! Real PTY job-control assertions shared by local and attached TUI tests.

use super::pty::{Shared, wait_for_screen_text, wait_until};
use std::{
    fs::File,
    io::Write,
    os::fd::AsRawFd,
    process::{Child, Command},
};

pub fn spawn(command: Command) -> (File, Child, std::sync::Arc<Shared>) {
    let mut shell = Command::new("/bin/bash");
    shell.env_clear().envs(
        command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value))),
    );
    shell.env("PS1", "JOB-PROMPT> ").env("PS2", "");
    shell.args([
        "--noprofile",
        "--norc",
        "-i",
        "-c",
        "\"$@\"; while [ -n \"$(jobs -p)\" ]; do IFS= read -r line || exit 1; eval \"$line\"; done",
        "otto-job",
    ]);
    shell.arg(command.get_program()).args(command.get_args());
    super::pty::spawn(shell)
}

pub fn suspend_resume(master: &mut File, child: &Child, shared: &Shared, draft: &str) {
    master
        .write_all(draft.as_bytes())
        .expect("type unsent draft");
    cycle(master, child, shared, draft);
    master.write_all(b"-resumed").expect("edit resumed draft");
    wait_for_screen_text(shared, &format!("{draft}-resumed"));
    master
        .write_all(b"\x15")
        .expect("clear draft without submitting");
}

pub fn cycle(master: &mut File, _child: &Child, shared: &Shared, expected: &str) {
    wait_for_screen_text(shared, expected);
    assert!(!canonical(master), "TUI must start in raw mode");
    let output_start = shared.raw.lock().unwrap().len();
    master.write_all(b"\x1a").expect("Ctrl-Z");
    wait_until(shared, "job stopped by Ctrl-Z", |shared| {
        let output = shared.raw.lock().unwrap();
        output[output_start..]
            .windows(7)
            .any(|window| window == b"Stopped")
    });
    assert!(
        canonical(master),
        "Ctrl-Z must restore cooked terminal mode"
    );
    {
        let output = shared.raw.lock().unwrap();
        let output = &output[output_start..];
        for reset in [b"\x1b[?2004l".as_slice(), b"\x1b[?1000l", b"\x1b[?1049l"] {
            assert!(
                output.windows(reset.len()).any(|window| window == reset),
                "missing terminal reset {reset:?}"
            );
        }
    }
    master
        .write_all(b"bg; wait %1; echo BG-CHECK\n")
        .expect("shell bg");
    wait_for_screen_text(shared, "BG-CHECK");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(canonical(master), "bg must not reclaim the shell terminal");
    let output_start = shared.raw.lock().unwrap().len();
    master.write_all(b"fg\n").expect("shell fg");
    wait_until(shared, "raw terminal restored after fg", |_| {
        !canonical(master)
    });
    wait_until(shared, "full foreground redraw", |shared| {
        let output = shared.raw.lock().unwrap();
        output[output_start..]
            .windows(4)
            .any(|window| window == b"\x1b[2J")
    });
    wait_for_screen_text(shared, expected);
}

#[allow(unsafe_code)]
fn canonical(master: &File) -> bool {
    // SAFETY: tcgetattr writes a termios value to valid memory on success.
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(master.as_raw_fd(), attributes.as_mut_ptr()) },
        0
    );
    // SAFETY: successful tcgetattr initialized the value.
    let attributes = unsafe { attributes.assume_init() };
    attributes.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG)
        == (libc::ICANON | libc::ECHO | libc::ISIG)
}
