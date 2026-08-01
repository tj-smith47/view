//! The Windows side of the engine's stdin channel: the pipe itself, and the
//! one question the outbox's inline fast path has to ask about it.
//!
//! Unix answers "can this write complete now" with a zero-timeout
//! `poll(POLLOUT)` on the child's stdin. Windows has no readiness poll for a
//! pipe, but it has something stronger: a pipe file object carries an exact
//! count of the bytes its buffer can still take, and
//! `NtQueryInformationFile(FilePipeLocalInformation)` reads it as
//! `WriteQuotaAvailable`. That is a byte count rather than a bit, so it
//! answers for the *specific* message rather than for some platform-fixed
//! bound, and it removes the need for a `PIPE_BUF`-style size cap: a write
//! into a pipe with room for all of it does not wait, and the Win32 contract
//! for a blocking-mode write is exactly that it waits only when the buffer
//! cannot take the remaining bytes.
//!
//! Getting to ask at all is what costs something. The stdin pipe
//! `Stdio::piped()` builds is opened `SYNCHRONIZE | GENERIC_WRITE`, and
//! neither of those includes `FILE_READ_ATTRIBUTES`, so the query on that
//! handle fails with `STATUS_ACCESS_DENIED` (0xC0000022) -- measured, not
//! assumed. Nothing recovers the access afterwards: `DuplicateHandle` asking
//! for more than the source handle holds fails with `ERROR_ACCESS_DENIED`,
//! and `ReOpenFile` fails with `ERROR_PIPE_BUSY` because the pipe permits a
//! single instance and the child already occupies it. So the channel is
//! built here instead, by `CreatePipe`, whose write handle answers the query
//! -- which is only possible at all because "anonymous pipes are implemented
//! using a named pipe with a unique name", so a named-pipe query accepts one.
//!
//! [`SyncPipe`] is what keeps the rest from being facts a reader has to
//! remember. Two properties of that handle are load-bearing, neither is
//! visible in a bare `OwnedHandle`, and both hold for a `CreatePipe` handle
//! and not for the one `Stdio::piped()` returns -- so the type is
//! constructible from this module's own creation path and nowhere else.
//!
//! **The handle is opened for synchronous I/O.** A query on an asynchronous
//! file object may return `STATUS_PENDING` and complete afterwards, writing
//! into the `IO_STATUS_BLOCK` and the information buffer once the call has
//! returned. Both are stack locals here, so on an overlapped handle the call
//! would be writing into a dead frame while the success check discarded the
//! result. That is the memory-safety precondition of every query below, and
//! no handle that could violate it can become a [`SyncPipe`].
//!
//! **Synchronous also means serialized.** Every operation on a synchronous
//! file object queues behind the one in flight, so a query raised while
//! another thread sat inside a write to a full pipe would block until that
//! write completed -- measured. What makes the query safe to raise from the
//! runtime loop is therefore not the query but where it is raised:
//! [`crate::outbox`] asks only while holding the same lock every write to
//! this pipe is made under, so there is never an operation in flight to
//! queue behind.

use std::io;
use std::os::windows::io::{AsRawHandle, HandleOrInvalid, OwnedHandle, RawHandle};

use windows_sys::Wdk::Storage::FileSystem::{
    FilePipeLocalInformation, NtQueryInformationFile, FILE_PIPE_LOCAL_INFORMATION,
};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// Bytes of buffer asked of `CreatePipe` for the engine's stdin.
///
/// The same 64 KiB the standard library gives a `Stdio::piped()` child, so
/// owning the pipe here changes what the inline path may ask and nothing
/// about how much the engine can fall behind before a write has to wait.
/// `CreatePipe` documents the size as a suggestion; the quota the fast path
/// reads is whatever the system actually granted, never this number.
const STDIN_CAPACITY: u32 = 64 * 1024;

/// One end of a pipe this module created, and so one known to be open for
/// synchronous I/O.
///
/// That property is a memory-safety precondition of the queries below rather
/// than a preference (see the module docs), so it is carried by a type no
/// foreign handle can be turned into instead of by a comment asking callers
/// to check.
pub(crate) struct SyncPipe(OwnedHandle);

impl SyncPipe {
    /// A second handle on the same pipe, for a caller that must ask about the
    /// pipe without borrowing the writer it would then write through.
    ///
    /// # Errors
    ///
    /// Returns the OS error `DuplicateHandle` failed with.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    /// Bytes the pipe can still accept without waiting, or `None` if the
    /// handle did not answer.
    ///
    /// A `None` is not a zero: it says the question went unanswered, and a
    /// caller owes the message to the writer thread either way.
    pub(crate) fn write_quota(&self) -> Option<u32> {
        self.local_info().map(|info| info.WriteQuotaAvailable)
    }

    /// Bytes the pipe's buffer holds in total, for tests that need the bound
    /// the quota can never exceed.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> Option<u32> {
        self.local_info().map(|info| info.OutboundQuota)
    }

    /// Reads the pipe's local end information, or `None` on any failure
    /// status.
    fn local_info(&self) -> Option<FILE_PIPE_LOCAL_INFORMATION> {
        let len = u32::try_from(size_of::<FILE_PIPE_LOCAL_INFORMATION>()).ok()?;
        let mut status_block = IO_STATUS_BLOCK::default();
        let mut info = FILE_PIPE_LOCAL_INFORMATION::default();
        // SAFETY: a `SyncPipe` exists only for a handle this module opened for
        // synchronous I/O, so the call completes before it returns and cannot
        // write into `status_block` or `info` afterwards -- which is what makes
        // stack locals sound out-params here. The handle is borrowed for the
        // call, and `len` is the size of the very buffer being passed, which is
        // the one the `FilePipeLocalInformation` class writes.
        #[allow(unsafe_code)]
        let status = unsafe {
            NtQueryInformationFile(
                self.0.as_raw_handle() as HANDLE,
                &mut status_block,
                std::ptr::addr_of_mut!(info).cast(),
                len,
                FilePipeLocalInformation,
            )
        };
        (status == 0).then_some(info)
    }
}

impl From<SyncPipe> for std::fs::File {
    fn from(pipe: SyncPipe) -> Self {
        Self::from(pipe.0)
    }
}

/// Creates the engine's stdin channel, returning the read end the child is
/// given and the write end the spawning process keeps.
///
/// # Errors
///
/// Returns the OS error `CreatePipe` failed with, or an
/// [`io::Error::other`] if it reports success while handing back something
/// that is not a handle.
pub(crate) fn child_stdin_pipe() -> io::Result<(OwnedHandle, SyncPipe)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: both out-params are live locals for the length of the call, and
    // a null `lpPipeAttributes` is the documented way to ask for a default
    // security descriptor and non-inheritable handles -- inheritance is the
    // spawn's business, which duplicates the read end inheritably itself.
    #[allow(unsafe_code)]
    let created = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), STDIN_CAPACITY) };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // claimed through the type that rejects the sentinel rather than on the
    // strength of the success code alone: `CreatePipe` documents its
    // out-params as indeterminate whenever it does not succeed, so the
    // difference between "success" and "a handle" is worth being checked
    // rather than assumed.
    // SAFETY: `CreatePipe` reported success, so each variable holds a handle
    // this process is now the sole owner of, and each is wrapped exactly once.
    #[allow(unsafe_code)]
    let claimed = unsafe {
        (
            HandleOrInvalid::from_raw_handle(read as RawHandle),
            HandleOrInvalid::from_raw_handle(write as RawHandle),
        )
    };
    let invalid = || io::Error::other("CreatePipe reported success with an invalid handle");
    let read = OwnedHandle::try_from(claimed.0).map_err(|_| invalid())?;
    let write = OwnedHandle::try_from(claimed.1).map_err(|_| invalid())?;
    Ok((read, SyncPipe(write)))
}
