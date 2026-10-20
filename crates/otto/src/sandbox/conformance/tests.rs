use super::*;

/// A child that has been signalled and has left the process table
/// satisfies the check.
#[tokio::test]
async fn assert_gone_accepts_a_signalled_child() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id() as i32;
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGKILL)
        .expect("signal the child");
    let _ = child.wait();

    assert_gone(pid).await;
}

/// Linux only: a child that was killed but not yet reaped is a zombie,
/// which the check accepts because it executes nothing.
///
/// This is the case that arises in the contract's own scenarios there,
/// where the descendant is an orphan waiting on PID 1. It cannot arise on
/// macOS, where launchd reaps an orphan promptly and [`is_zombie`] is
/// always false — which is why this test would fail there: its child's
/// parent is the test process itself, so nothing reaps it until the line
/// after the assertion.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn assert_gone_accepts_an_unreaped_zombie() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id() as i32;
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGKILL)
        .expect("signal the child");
    // Settle into the zombie state the check is meant to accept.
    while !is_zombie(pid) {
        tokio::time::sleep(SETTLE_INTERVAL).await;
    }

    assert_gone(pid).await;

    let _ = child.wait();
}

/// The check this contract exists for: a descendant nothing signalled is
/// a survivor, and no settle window may excuse it.
///
/// The verdict is taken from a task rather than `#[should_panic]` so the
/// child can be reaped afterwards; `assert_gone` kills it on its way out,
/// and this waits on it, so the test leaks neither a process nor a zombie.
#[tokio::test]
async fn assert_gone_rejects_a_survivor() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id() as i32;

    let verdict = tokio::spawn(async move { assert_gone(pid).await }).await;

    assert!(
        verdict.is_err_and(|error| error.is_panic()),
        "a process nothing signalled must fail the check"
    );
    let _ = child.wait();
}
