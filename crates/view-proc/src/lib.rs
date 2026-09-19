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

/// Where a refused tie is written down, or nowhere until a process says.
static REPORT: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Refusals raised before a writer arrived, held until one does.
///
/// Every arm this crate ties with can be refused while the process is still
/// starting up -- the watcher's pipe and its `/bin/sh` are made from the
/// first line of `main` -- so the refusal a session most needs to read is
/// the one raised before any log existed to write it to. Dropped, a host
/// that refuses the tie outright looks exactly like one that ties every
/// child.
static PENDING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// How many of those are kept. Each arm refuses once per process, and a
/// run that reaches this many has already said what a reader needs; a
/// bound is what keeps a refusal in a loop from growing without one.
const PENDING_CAP: usize = 8;

/// Hands this crate somewhere to record a tie the host refused.
///
/// Every arm can be refused -- a `prctl` a seccomp profile answers `EPERM`,
/// a host with no `/bin/sh`, a job object an older Windows will not nest --
/// and every refusal leaves that child running and untied rather than
/// unspawned, because an editor with no engine is the worse answer. What
/// makes the trade dangerous is that it is silent: nothing in the process
/// table says which children are tied, so a session running one loose looks
/// exactly like a session running none.
///
/// A handed-over writer rather than a logging dependency: nothing here
/// depends on any other crate in this workspace, which is what lets every
/// crate that spawns depend on this one. The process that has a log gives
/// it one, from `main` and before it spawns; a process that never calls
/// this records nothing, which is what a test binary and a bench driver do.
///
/// The first caller wins, so a second call is a no-op rather than a
/// replacement.
///
/// Whatever was refused before this call is written out here, so the order
/// of the two lines in a `main` decides nothing.
pub fn record_refusals_with(report: fn(&str)) {
    // the lock is held across the install so that a refusal racing it is
    // either buffered before the drain below or written straight out after
    // it, never pushed onto a list nothing reads again
    let Ok(mut held) = PENDING.lock() else {
        let _ = REPORT.set(report);
        return;
    };
    if REPORT.set(report).is_err() {
        return;
    }
    // the lock is released before the writer runs: a writer that reached
    // back into this crate would otherwise be waiting on itself
    let notes = std::mem::take(&mut *held);
    drop(held);
    for note in notes {
        report(&note);
    }
}

/// Writes one refusal wherever [`record_refusals_with`] pointed, or holds
/// it for the writer that has not arrived yet.
#[cfg(any(unix, windows))]
fn refused(note: &str) {
    if let Ok(mut pending) = PENDING.lock() {
        if REPORT.get().is_none() {
            if pending.len() < PENDING_CAP {
                pending.push(note.to_string());
            }
            return;
        }
    }
    if let Some(report) = REPORT.get() {
        report(note);
    }
}

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
        // a refused job leaves this one child running and untied, which is
        // the trade every arm here makes; `tie_spawned_child` is what writes
        // the refusal down, so there is nothing further to do with the
        // answer at this call
        let _tied = tie_spawned_child(child.id());
        Ok(child)
    }
    #[cfg(not(any(unix, windows)))]
    {
        spawn(&mut command)
    }
    #[cfg(target_os = "linux")]
    {
        let Some(anchor) = anchor() else {
            refused("no thread to fork from: this child runs untied");
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
/// the one the startup budget measures.
///
/// The pipe that watcher reads is made here, on the calling thread, and
/// only the fork goes to a thread of its own. `std::io::pipe` is two calls
/// where there is no `pipe2` -- the pair, then the `FD_CLOEXEC` -- and a
/// fork landing between them inherits the write end and holds the pipe open
/// for the life of the session, which is a watcher that never reads EOF and
/// never fires. Made from `main` there is no second thread in existence to
/// fork; made on a thread it would run beside whatever `main` does next,
/// and what `main` does next is fork (the ssh probe, the clipboard helper).
pub fn prepare_to_tie_children() {
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = WATCHER_IN.get_or_init(|| start_watcher(fork_on_a_thread));
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
///
/// The refusal is written down through [`record_refusals_with`] here rather
/// than at the call site: a caller of this function is a crate that spawns,
/// which is a crate with no logger of its own, and the three shapes a
/// refusal takes are known here and nowhere else.
/// The answer is still `#[must_use]`, for a caller that can do more with it
/// than run on.
#[cfg(windows)]
#[must_use]
pub fn tie_spawned_child(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    let Some(job) = killing_job() else {
        refused("no job object: this host refused to make one, so every tied child runs untied");
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
            refused(&format!("pid {pid} runs untied: it could not be opened"));
            return false;
        }
        let tied = AssignProcessToJobObject(std::ptr::with_exposed_provenance_mut(job), child) != 0;
        CloseHandle(child);
        if !tied {
            refused(&format!("pid {pid} runs untied: the job object refused it"));
        }
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
/// Every failure it can meet -- a host with no `/bin/sh`, a pipe the kernel
/// refused, a watcher already gone -- leaves this child running and untied
/// and is written down rather than raised. An editor that will not start is
/// a worse answer than a child that can be orphaned, which is the same trade
/// the Linux arm makes with a refused `prctl`.
#[cfg(all(unix, not(target_os = "linux")))]
fn watch_for_this_process_ending(pid: u32) {
    use std::io::Write;

    let Some(pipe) = watcher() else {
        refused(&format!("pid {pid} runs untied: this host has no watcher"));
        return;
    };
    let Ok(mut pipe) = pipe.lock() else {
        refused(&format!(
            "pid {pid} runs untied: the watcher's pipe is held by a panicked thread"
        ));
        return;
    };
    if writeln!(pipe, "{pid}").is_err() {
        refused(&format!(
            "pid {pid} runs untied: the watcher is gone from the other end of its pipe"
        ));
    }
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
static WATCHER_IN: std::sync::OnceLock<Option<std::sync::Mutex<std::io::PipeWriter>>> =
    std::sync::OnceLock::new();

/// The write end, built here when nothing called
/// [`prepare_to_tie_children`] first -- a test binary, a bench or oracle
/// driver, anything whose `main` is not view's.
///
/// What that costs is stated rather than closed: the pipe is made on
/// whichever thread reached the first tied spawn, and a binary that never
/// calls the entry point above is one that already has threads running
/// (libtest runs its cases on them). Where there is no `pipe2` the pair and
/// its `FD_CLOEXEC` are two calls, so a fork by another of those threads
/// landing between them inherits the write end, and a write end nothing
/// closes is a pipe that never reads EOF and a watcher that never fires.
/// The window is those two calls and not the whole watcher start -- the
/// fork of `/bin/sh` below happens after the descriptor is already
/// close-on-exec -- and closing it entirely takes the call from `main` that
/// [`prepare_to_tie_children`] is.
#[cfg(all(unix, not(target_os = "linux")))]
fn watcher() -> Option<&'static std::sync::Mutex<std::io::PipeWriter>> {
    WATCHER_IN.get_or_init(|| start_watcher(fork_here)).as_ref()
}

/// Makes the pipe on the calling thread, then hands the read end to `fork`.
///
/// The split is the whole point: the pipe is the half that cannot be moved
/// off a thread that may fork, and the `/bin/sh` is the half worth moving
/// off the first tied spawn.
#[cfg(all(unix, not(target_os = "linux")))]
fn start_watcher(fork: fn(std::io::PipeReader)) -> Option<std::sync::Mutex<std::io::PipeWriter>> {
    let Ok((reader, writer)) = std::io::pipe() else {
        refused("no watcher: this host refused the pipe one reads, so tied children run untied");
        return None;
    };
    fork(reader);
    Some(std::sync::Mutex::new(writer))
}

/// Forks the watcher on the calling thread.
#[cfg(all(unix, not(target_os = "linux")))]
fn fork_here(reader: std::io::PipeReader) {
    let started = Command::new("/bin/sh")
        .arg("-c")
        .arg(WATCHER)
        .stdin(std::process::Stdio::from(reader))
        // never the terminal this process paints to: a watcher holding the
        // pty open outlives the session on screen by the length of its own
        // wait, and the session below it reads no hangup
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if started.is_err() {
        refused("no watcher: this host would not run /bin/sh, so tied children run untied");
    }
}

/// Forks the watcher on a thread of its own, so the cost lands beside
/// whatever the caller does next rather than inside the first tied spawn.
#[cfg(all(unix, not(target_os = "linux")))]
fn fork_on_a_thread(reader: std::io::PipeReader) {
    let started = std::thread::Builder::new()
        .name(String::from("view-proc-watcher"))
        .spawn(move || fork_here(reader));
    if started.is_err() {
        refused("no watcher: this process could not start the thread that forks it");
    }
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

#[cfg(all(test, any(unix, windows)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{record_refusals_with, refused, PENDING_CAP};

    /// What the writer installed below was handed.
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    fn remember(note: &str) {
        if let Ok(mut seen) = SEEN.lock() {
            seen.push(note.to_string());
        }
    }

    fn seen() -> Vec<String> {
        SEEN.lock().unwrap().clone()
    }

    /// A refusal raised before any writer exists reaches the one that
    /// arrives afterwards.
    ///
    /// The whole of the tie is decided in the first lines of a `main`, and
    /// the watcher's own pipe and `/bin/sh` are made there: a process that
    /// installed its writer one line later than it prepared the tie used to
    /// drop exactly the refusal that says no child of this session is tied.
    ///
    /// One case and one test binary, because the two statics it reads are
    /// per-process: a second case touching either would be deciding this
    /// one's answer from another thread.
    #[test]
    fn a_refusal_raised_before_any_writer_is_installed_is_not_lost() {
        for i in 0..PENDING_CAP + 2 {
            refused(&format!("refused {i}"));
        }
        assert!(
            seen().is_empty(),
            "a refusal reached a writer that had not been installed yet"
        );

        record_refusals_with(remember);

        assert_eq!(
            seen().len(),
            PENDING_CAP,
            "the held refusals did not arrive whole at the writer that \
             installed itself after them, or the bound on them did not hold"
        );
        assert_eq!(
            seen().first().map(String::as_str),
            Some("refused 0"),
            "the refusals arrived in some order other than the one they \
             were raised in"
        );

        refused("refused after the writer");
        assert_eq!(
            seen().len(),
            PENDING_CAP + 1,
            "a refusal raised after the writer was installed did not reach it"
        );
    }
}
