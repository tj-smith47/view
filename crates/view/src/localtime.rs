//! The host's UTC offset, for [`crate::runtime::dispatch`] to hand
//! [`view_core::model::Model::set_utc_offset`] ahead of every fold:
//! `view-core` stays pure, so it never asks the platform for this itself,
//! and every message stamp rendered UTC with nothing on screen or in the
//! docs saying so.
//!
//! Read fresh on every fold and never cached: a session that straddles a
//! DST change must stamp the new offset from the fold after the flip, with
//! no restart. `localtime_r` and `GetTimeZoneInformation` already read the
//! zone the OS keeps parsed, so this costs the one syscall `dispatch`
//! already pays each fold for `SystemTime::now`, with no zone parse.

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    /// A pin's own offset, standing in for the host's whenever it is set.
    static TEST_OFFSET_SECS: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Seconds east of UTC on this host, or a pin's injected stand-in for one.
#[must_use]
pub(crate) fn utc_offset_secs() -> i64 {
    #[cfg(test)]
    if let Some(secs) = TEST_OFFSET_SECS.with(Cell::get) {
        return secs;
    }
    platform_offset_secs()
}

/// Stands the next [`utc_offset_secs`] call in for the host's own reading,
/// or clears the stand-in on `None`. Test-only: what a pin uses to prove
/// `dispatch` re-reads the offset on every fold.
#[cfg(test)]
pub(crate) fn set_test_offset_secs(secs: Option<i64>) {
    TEST_OFFSET_SECS.with(|cell| cell.set(secs));
}

/// Arms [`set_test_offset_secs`] for the guard's own scope and clears it on
/// drop, so a test thread the harness reuses afterward reads the host's
/// own offset again, with no stale pin left -- including when the test
/// panics, since `Drop` still runs on unwind.
#[cfg(test)]
pub(crate) struct TestOffsetGuard;

#[cfg(test)]
impl TestOffsetGuard {
    pub(crate) fn new(secs: i64) -> Self {
        set_test_offset_secs(Some(secs));
        Self
    }
}

#[cfg(test)]
impl Drop for TestOffsetGuard {
    fn drop(&mut self) {
        set_test_offset_secs(None);
    }
}

#[cfg(unix)]
fn platform_offset_secs() -> i64 {
    // SAFETY: `t` is a plain stack value `libc::time` writes through a
    // valid pointer; `tm` is zero-initialized and `localtime_r` fills it
    // from `t` without retaining either pointer past the call.
    #[allow(unsafe_code)]
    let (t, mut tm): (libc::time_t, libc::tm) =
        unsafe { (libc::time(std::ptr::null_mut()), std::mem::zeroed()) };
    #[allow(unsafe_code)]
    let filled = unsafe { libc::localtime_r(&t, &mut tm) };
    if filled.is_null() {
        return 0;
    }
    // `tm_gmtoff`'s width is platform-defined (`c_long`): `i32` on some
    // targets, already `i64` on this one, so the cast is a no-op here and
    // necessary on those.
    #[allow(clippy::unnecessary_cast)]
    let secs = tm.tm_gmtoff as i64;
    secs
}

#[cfg(windows)]
fn platform_offset_secs() -> i64 {
    use windows_sys::Win32::System::Time::{
        GetTimeZoneInformation, TIME_ZONE_ID_DAYLIGHT, TIME_ZONE_ID_INVALID,
    };

    // SAFETY: `info` is a plain stack value the Win32 call fills in place;
    // no pointer it receives is retained past the call.
    #[allow(unsafe_code)]
    let mut info = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let id = unsafe { GetTimeZoneInformation(&mut info) };
    if id == TIME_ZONE_ID_INVALID {
        return 0;
    }
    // `Bias` is minutes UTC is *ahead* of local, the opposite sign of the
    // seconds-east-of-UTC this returns; `id` says which of the two
    // additional biases is in effect right now.
    let extra = if id == TIME_ZONE_ID_DAYLIGHT {
        info.DaylightBias
    } else {
        info.StandardBias
    };
    let minutes = i64::from(info.Bias) + i64::from(extra);
    -minutes * 60
}

#[cfg(not(any(unix, windows)))]
fn platform_offset_secs() -> i64 {
    0
}
