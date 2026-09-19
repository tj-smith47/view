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
/// | macOS, and every other unix | yes | a watcher process reading a pipe only this process holds the other end of; the pipe closes when this process does, however it ended |
/// | Windows | yes | a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` that each tied child is put in, and `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` so the children *it* starts are not |
///
/// Every arm ties the one child it was handed and nothing below it. An
/// engine that runs `jobstart`, `:terminal` or `:!start` owns what it
/// starts exactly as bare nvim does, on every platform.
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
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let child = spawn(&mut command)?;
        // after the spawn and not before it, because the pid is what the
        // watcher is told. A kill landing inside that window leaves this
        // one child unwatched, which is the same window the Linux arm's
        // getppid re-check closes and the one thing neither arm can hand to
        // a process that does not exist yet.
        watch_for_this_process_ending(child.id());
        Ok(child)
    }
    #[cfg(windows)]
    {
        let child = spawn(&mut command)?;
        tie_spawned_child(child.id());
        Ok(child)
    }
    #[cfg(not(any(unix, windows)))]
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

/// Builds what a tied spawn needs, so that the first spawn does not.
///
/// Optional, and idempotent. A process that never calls it gets the same
/// machinery from its first tied spawn, built on that spawn's own thread.
///
/// Call it from `main`, before the process starts any thread that spawns.
/// Off Linux the tie is a watcher process, so the first tied spawn forks a
/// `/bin/sh` -- and the first tied spawn is the engine, whose own cost is
/// the one the startup budget measures. The pipe handed to that watcher is
/// also two syscalls where there is no `pipe2`, and a fork landing between
/// them inherits the write end and holds it open forever; no other thread
/// spawning yet is what makes that unrepresentable rather than unlikely.
pub fn prepare_to_tie_children() {
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // on a thread because the fork is the cost being moved, not deleted
        let _ = std::thread::Builder::new()
            .name(String::from("view-proc-watcher"))
            .spawn(watcher);
    }
    #[cfg(target_os = "linux")]
    {
        let _ = anchor();
    }
    #[cfg(windows)]
    {
        let _ = killing_job();
    }
}

/// Puts an already-running child in the job object that ends what it holds
/// when this process ends, for a caller whose child cannot go through
/// [`spawn_tied_to_this_process`] -- a `tokio::process` spawn, which builds
/// and owns its own pipes.
///
/// `false` where the job could not be made, the child could not be opened,
/// or it could not be put in the job (a nested job an older Windows
/// refuses), which leaves that one child untied rather than unspawned.
///
/// A pid and not a handle, as on every other platform. Windows recycles a
/// pid only once the last handle to the process has closed, and the caller
/// is holding one -- it is the child it just spawned -- so the pid names
/// that child and no other for as long as the caller can make this call.
///
/// Windows has no stable way to hand a job to a child at creation on this
/// toolchain -- `CommandExt::raw_attribute`, which carries
/// `PROC_THREAD_ATTRIBUTE_JOB_LIST`, is unstable, and `CREATE_SUSPENDED`
/// needs the initial thread handle `std` does not return -- so the child is
/// assigned once it is running. What that leaves open is the window the
/// unix arms leave too: a kill landing between the spawn and the tie leaves
/// this one child loose.
#[cfg(windows)]
pub fn tie_spawned_child(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    let Some(job) = killing_job() else {
        return false;
    };
    // SAFETY: `job` is a job handle this process created and never closed,
    // and the handle opened below is checked before it is passed on and
    // closed on both paths out. The two rights asked for are the two
    // `AssignProcessToJobObject` documents needing.
    #[allow(unsafe_code)]
    unsafe {
        let child = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if child.is_null() {
            return false;
        }
        let tied = AssignProcessToJobObject(std::ptr::with_exposed_provenance_mut(job), child) != 0;
        CloseHandle(child);
        tied
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

/// The program the watcher process runs.
///
/// It reads pids, one per line, and records the identity the process table
/// reports for each. When the pipe it reads closes -- which is what this
/// process ending does, whatever ended it, `SIGKILL` included -- it kills
/// whichever of them are still there.
///
/// The identity is what makes that safe. A pid this process has already
/// waited on is free for the kernel to hand to somebody else, and a session
/// that restarts its engine frees one such pid per restart; killing the
/// list blind would eventually kill a stranger. Two fields answer that, and
/// they answer different halves of it:
///
/// - the parent, read while the process that wrote the line is still alive.
///   A pid already recycled by the time this reads it belongs to somebody
///   whose parent is not `$PPID`, and the line is dropped. Once `$PPID` is
///   gone the check is dropped instead of failed: a tied child outlives its
///   parent by exactly the moment this exists for, and by then it has been
///   reparented.
/// - the start time, compared again before the kill. `ps` reports the second
///   a process started, so a pid whose start time still matches the one
///   recorded is the same process and no other.
///
/// The window neither closes: a pid recycled *after* this read and before
/// the kill, into a process that started in the same second the recorded
/// one did. Reaching it takes a reaped child, a full turn of the pid space
/// and a one-second collision, in that order.
///
/// It ignores the signals a terminal sends its foreground group: a Ctrl-C
/// reaches this process and the editor above it at once, and a watcher that
/// died of it would leave the rest of that session untied.
#[cfg(all(unix, not(target_os = "linux")))]
const WATCHER: &str = r#"
trap '' INT TERM HUP QUIT
count=0
while IFS= read -r pid; do
  case $pid in
    (''|*[!0-9]*) continue ;;
  esac
  set -- $(ps -o ppid=,lstart= -p "$pid" 2>/dev/null)
  [ "$#" -gt 1 ] || continue
  if [ "$1" != "$PPID" ] && kill -0 "$PPID" 2>/dev/null; then
    continue
  fi
  shift
  count=$((count + 1))
  eval "pid$count=\$pid; started$count=\"\$*\""
done
seen=0
while [ "$seen" -lt "$count" ]; do
  seen=$((seen + 1))
  eval "pid=\$pid$seen; started=\$started$seen"
  set -- $(ps -o lstart= -p "$pid" 2>/dev/null)
  if [ "$#" -gt 0 ] && [ "$*" = "$started" ]; then
    kill -9 "$pid" 2>/dev/null
  fi
done
exit 0
"#;

/// Tells the watcher to end `pid` when this process ends, starting the
/// watcher if this is the first tied spawn.
///
/// Quiet about every failure it can meet: a host with no `/bin/sh`, a pipe
/// the kernel refused, a watcher already gone. An editor that will not
/// start is a worse answer than a child that can be orphaned, which is the
/// same trade the Linux arm makes with a refused `prctl`.
#[cfg(all(unix, not(target_os = "linux")))]
fn watch_for_this_process_ending(pid: u32) {
    use std::io::Write;

    let Some(pipe) = watcher() else {
        return;
    };
    let Ok(mut pipe) = pipe.lock() else {
        return;
    };
    let _ = writeln!(pipe, "{pid}");
}

/// The pipe the watcher reads, kept for the life of the process.
///
/// A static and never a value a caller holds: the pipe closing is the whole
/// mechanism, so the write end has to outlive every other thing in this
/// process and close exactly when the process does. Statics are not
/// dropped, so the kernel closing it at exit is what the watcher sees.
///
/// The read end reaches only the watcher: it is moved into the spawn as
/// that child's standard input, and the write end carries `FD_CLOEXEC`
/// (every pipe `std::io::pipe` makes does), so no child spawned afterwards
/// holds the pipe open against this process's own ending.
#[cfg(all(unix, not(target_os = "linux")))]
fn watcher() -> Option<&'static std::sync::Mutex<std::io::PipeWriter>> {
    static WATCHER_IN: std::sync::OnceLock<Option<std::sync::Mutex<std::io::PipeWriter>>> =
        std::sync::OnceLock::new();
    WATCHER_IN.get_or_init(start_watcher).as_ref()
}

/// Starts the watcher, or reports that this host has none.
#[cfg(all(unix, not(target_os = "linux")))]
fn start_watcher() -> Option<std::sync::Mutex<std::io::PipeWriter>> {
    let (reader, writer) = std::io::pipe().ok()?;
    Command::new("/bin/sh")
        .arg("-c")
        .arg(WATCHER)
        .stdin(std::process::Stdio::from(reader))
        // never the terminal this process paints to: a watcher holding the
        // pty open outlives the session on screen by the length of its own
        // wait, and the session below it reads no hangup
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    Some(std::sync::Mutex::new(writer))
}

/// The job object every tied child is put in, created on the first tie and
/// never closed.
///
/// The handle is deliberately never closed. It is the only handle to the
/// job, so the job stays open exactly as long as this process does and
/// closes when the kernel closes what the process held -- whether the
/// process left by its own exit or by a `TerminateProcess` that ran no
/// code at all. Closing it is what kills what it holds.
///
/// Carried as a `usize` because a raw handle is a pointer, which a static
/// may not hold across threads, and this one is read from every thread that
/// spawns.
#[cfg(windows)]
fn killing_job() -> Option<usize> {
    static JOB: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *JOB.get_or_init(create_killing_job)
}

/// Creates the job and sets it to kill what it holds when it closes, and to
/// let what its members spawn out of it. `None` where either was refused,
/// which leaves the children untied rather than unspawned.
///
/// This process is *not* a member. A job holding the editor holds every
/// child of it that ever runs -- a `git`, a clipboard helper, and every
/// process the engine starts for `jobstart`, `:terminal` or `:!start` --
/// which is a scope no other platform's arm has and which bare nvim does
/// not have either. `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` says the same
/// thing one level down, for what a tied child starts itself: the engine's
/// own children are its business, exactly as they are under
/// `PR_SET_PDEATHSIG`. That half is not observable through nvim, which puts
/// every child it spawns in a job of libuv's own that closes with it -- a
/// `jobstart` child goes with a terminated bare nvim on Windows too -- so
/// the flag states the scope rather than changing what nvim does.
#[cfg(windows)]
fn create_killing_job() -> Option<usize> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    };

    let Ok(size) = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()) else {
        return None;
    };
    // SAFETY: both calls are made with the arguments their documented
    // contracts ask for -- a zeroed limit structure of the size passed
    // beside it, and a handle this function created -- and the first result
    // is checked before the second call reads it.
    #[allow(unsafe_code)]
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size,
        ) == 0
        {
            CloseHandle(job);
            return None;
        }
        Some(job.expose_provenance())
    }
}
