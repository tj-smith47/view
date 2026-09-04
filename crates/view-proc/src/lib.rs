//! One door for every long-lived child this workspace spawns.
//!
//! A `Drop` covers the exits a process chooses and none of the ones it does
//! not: a `SIGKILL`, a harness timeout or a restarted session leaves the
//! child an orphan, and a child wedged past noticing its closed pipes never
//! leaves on its own. Three of them were found reparented to init at 100%
//! CPU for days.
//!
//! A leaf crate rather than a module of the engine: the editor, the agent
//! adapter and the measurement harnesses all spawn children of that shape,
//! and `scripts/audit-deps.sh` forbids most of them an edge to each other.
//! Nothing here depends on any other crate in the workspace, so every one of
//! them may depend on this.
#![forbid(unsafe_op_in_unsafe_fn)]

use std::process::{Child, Command};

/// Spawns `command` as a child the operating system ends when this process
/// does.
///
/// | platform | covered | mechanism |
/// |---|---|---|
/// | Linux | yes | `PR_SET_PDEATHSIG`, armed in the child between fork and exec |
/// | macOS | no | kqueue's `NOTE_EXIT` needs a watcher that outlives the kill creating the orphan -- a third process no caller here has |
/// | Windows | no | coverable by a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which no caller here creates |
///
/// Off Linux this is an ordinary spawn and the child outlives a parent
/// killed outright, leaving the owner's `Drop` as the only thing that reaps
/// it.
///
/// A spawn rather than a step a caller applies to its own [`Command`]: the
/// parent-death signal names the *thread* that forked the child, so arming
/// it on a thread that returns before the child is wanted kills the child
/// there. This entry point forks where that cannot happen.
///
/// # Errors
///
/// Whatever the spawn reports, plus [`std::io::ErrorKind::Other`] if the
/// thread the fork is handed to could not be reached.
pub fn spawn_tied_to_this_process(command: Command) -> std::io::Result<Child> {
    spawn_tied_with(command, Command::spawn)
}

/// [`spawn_tied_to_this_process`] for a caller whose spawn is more than one
/// call -- a retry, a fixture check -- which must happen on the same thread
/// the fork does.
///
/// A function pointer rather than a closure: it crosses to the thread below,
/// and a spawn worth retrying is a named function rather than a capture.
///
/// # Errors
///
/// As [`spawn_tied_to_this_process`].
pub fn spawn_tied_with(
    mut command: Command,
    spawn: fn(&mut Command) -> std::io::Result<Child>,
) -> std::io::Result<Child> {
    #[cfg(not(target_os = "linux"))]
    {
        spawn(&mut command)
    }
    #[cfg(target_os = "linux")]
    {
        let Some(anchor) = anchor() else {
            return spawn(&mut command);
        };
        arm_parent_death_at_spawn(&mut command);
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let gone = || std::io::Error::other("the thread that forks tied children is gone");
        anchor
            .send((command, spawn, reply_tx))
            .map_err(|_| gone())?;
        reply_rx.recv().map_err(|_| gone())?
    }
}

/// What one spawn hands to the thread below, and where the answer goes.
#[cfg(target_os = "linux")]
type Fork = (
    Command,
    fn(&mut Command) -> std::io::Result<Child>,
    std::sync::mpsc::SyncSender<std::io::Result<Child>>,
);

/// The thread every tied fork is made from, created on the first spawn and
/// never joined.
///
/// `PR_SET_PDEATHSIG` names the thread that forked the child, not the
/// process: the signal is delivered when *that thread* exits, whether or not
/// the process behind it is still running (`prctl(2)`). view spawns its
/// engine from a short-lived background attach thread, and arming the signal
/// there killed every engine the moment the attach finished -- the session
/// then restarted against the swap file the dead child had left and painted
/// `E305` instead of a buffer. A thread that lives as long as the process is
/// what makes "when the parent thread exits" mean "when this process ends".
///
/// `None` where the thread could not be created, which leaves the signal
/// unarmed rather than armed against the caller's own thread: a child that
/// can be orphaned is the defect this closes, and a child killed the moment
/// its caller's thread returns is a worse one.
#[cfg(target_os = "linux")]
fn anchor() -> Option<&'static std::sync::mpsc::SyncSender<Fork>> {
    static ANCHOR: std::sync::OnceLock<Option<std::sync::mpsc::SyncSender<Fork>>> =
        std::sync::OnceLock::new();
    ANCHOR
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Fork>(0);
            std::thread::Builder::new()
                .name(String::from("view-proc-fork"))
                .spawn(move || {
                    while let Ok((mut command, spawn, reply)) = rx.recv() {
                        let _ = reply.send(spawn(&mut command));
                    }
                })
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}

/// Adds the `pre_exec` closure that arms the parent-death signal, reading
/// the parent pid here rather than in the child so the child has a value to
/// compare its own `getppid` against.
#[cfg(target_os = "linux")]
fn arm_parent_death_at_spawn(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    let parent = rustix::process::getpid().as_raw_pid();
    // SAFETY: `arm_parent_death` calls only `prctl` and `getppid`, both raw
    // syscalls with no allocator and no lock behind them -- the constraint
    // `pre_exec` imposes on code running between `fork` and `exec` in the
    // child.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || arm_parent_death(parent));
    }
}

/// Asks the kernel to `SIGKILL` this child when its parent thread exits,
/// then refuses the exec if the parent it was armed against is already gone.
///
/// The refusal closes the fork/exec race: a parent that died between the
/// fork and the `prctl` leaves a child whose signal will never fire, and a
/// child that never execs is one nothing has to reap.
///
/// A `prctl` the host refuses (a seccomp profile answering `EPERM` or
/// `ENOSYS`) leaves the child unarmed rather than unspawned: an editor with
/// no engine is a worse answer than an engine that can be orphaned.
#[cfg(target_os = "linux")]
fn arm_parent_death(expected: rustix::process::RawPid) -> std::io::Result<()> {
    let _ = rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL));
    let parent = rustix::process::getppid().map(rustix::process::Pid::as_raw_pid);
    if parent != Some(expected) {
        return Err(std::io::Error::from_raw_os_error(
            rustix::io::Errno::SRCH.raw_os_error(),
        ));
    }
    Ok(())
}
