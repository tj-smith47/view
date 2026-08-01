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
//! The query is not free of the pipe's own synchronisation: a handle from
//! `CreatePipe` is synchronous, and every operation on a synchronous file
//! object queues behind the one in flight. A query issued while another
//! thread sits inside a write to a full pipe therefore blocks until that
//! write completes -- also measured. What makes the query safe to call from
//! the runtime loop is not the query but the caller: [`crate::outbox`] asks
//! only while holding the same lock every write to this pipe is made under,
//! so there is never an operation in flight to queue behind.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

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

/// Creates the engine's stdin channel, returning the read end the child is
/// given and the write end the spawning process keeps.
///
/// # Errors
///
/// Returns the OS error `CreatePipe` failed with.
// FFI is the whole of this module: the readiness answer the inline path
// needs exists only behind ntdll and kernel32 entry points.
#[allow(unsafe_code)]
pub(crate) fn child_stdin_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: both out-params are live locals for the length of the call, and
    // a null `lpPipeAttributes` is the documented way to ask for a default
    // security descriptor and non-inheritable handles -- inheritance is the
    // spawn's business, which duplicates the read end inheritably itself.
    let created = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), STDIN_CAPACITY) };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `CreatePipe` reported success, so both variables hold handles
    // this process is now the sole owner of, and each is wrapped exactly once.
    let ends = unsafe {
        (
            OwnedHandle::from_raw_handle(read as RawHandle),
            OwnedHandle::from_raw_handle(write as RawHandle),
        )
    };
    Ok(ends)
}

/// Bytes the pipe behind `handle` can still accept without waiting, or
/// `None` if the handle cannot answer.
///
/// A `None` is not a zero: it says the question was not answered, and the
/// caller owes the message to the writer thread either way.
pub(crate) fn write_quota(handle: &OwnedHandle) -> Option<u32> {
    local_info(handle).map(|info| info.WriteQuotaAvailable)
}

/// Bytes the pipe behind `handle` can hold in total, for tests that need
/// the bound the quota can never exceed.
#[cfg(test)]
pub(crate) fn capacity(handle: &OwnedHandle) -> Option<u32> {
    local_info(handle).map(|info| info.OutboundQuota)
}

/// Reads the pipe's local end information, or `None` on any failure status.
// see the module docs: this is the readiness answer, and there is no safe
// binding for it.
#[allow(unsafe_code)]
fn local_info(handle: &OwnedHandle) -> Option<FILE_PIPE_LOCAL_INFORMATION> {
    let len = u32::try_from(size_of::<FILE_PIPE_LOCAL_INFORMATION>()).ok()?;
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut info = FILE_PIPE_LOCAL_INFORMATION::default();
    // SAFETY: the handle is borrowed for the call, both out-params are live
    // locals, and `len` is the size of the very buffer being passed, which is
    // the one the `FilePipeLocalInformation` class writes.
    let status = unsafe {
        NtQueryInformationFile(
            handle.as_raw_handle() as HANDLE,
            &mut status_block,
            std::ptr::addr_of_mut!(info).cast(),
            len,
            FilePipeLocalInformation,
        )
    };
    (status == 0).then_some(info)
}
