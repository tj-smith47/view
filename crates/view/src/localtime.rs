//! The host's UTC offset, for [`crate::runtime::dispatch`] to hand
//! [`view_core::model::Model::set_utc_offset`] ahead of every fold (I12):
//! `view-core` stays pure, so it never asks the platform for this itself,
//! and every message stamp rendered UTC with nothing on screen or in the
//! docs saying so.
//!
//! Read once and cached: a host's offset changes only across a DST flip a
//! running session is unlikely to straddle, and every fold re-reading it
//! from the OS would be a syscall this loop otherwise never pays per
//! message.

use std::sync::OnceLock;

static OFFSET_SECS: OnceLock<i64> = OnceLock::new();

/// Seconds east of UTC on this host, cached after the first call.
#[must_use]
pub(crate) fn utc_offset_secs() -> i64 {
    *OFFSET_SECS.get_or_init(platform_offset_secs)
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
