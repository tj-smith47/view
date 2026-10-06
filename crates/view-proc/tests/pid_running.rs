//! `pid_running` on the platform the suite runs on.

use std::process::{Command, Stdio};

use view_proc::pid_running;

#[test]
fn this_process_is_running() {
    assert!(pid_running(std::process::id()));
}

#[test]
fn a_child_that_has_exited_is_not_running() -> std::io::Result<()> {
    // The test binary itself, asked only to list its tests, is the one
    // short-lived program every platform is sure to have.
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--list")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let pid = child.id();
    child.wait()?;
    assert!(
        !pid_running(pid),
        "pid {pid} read as running after its exit"
    );
    drop(child);
    assert!(!pid_running(pid), "pid {pid} read as running once released");
    Ok(())
}

#[test]
fn pid_zero_is_not_a_running_process() {
    assert!(!pid_running(0));
}
