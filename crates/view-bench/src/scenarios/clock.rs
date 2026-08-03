//! A monotonic nanosecond clock shared by the measurement scenarios.
//!
//! On unix the tap channel ([`super::taps`]) compares harness timestamps
//! against the raw `CLOCK_MONOTONIC` nanoseconds the instrumented view child
//! writes across a FIFO, so the harness must read that same clock rather than
//! std's opaque `Instant`. Off unix there is no tap channel (it is
//! `#[cfg(unix)]`) and the only portable caller -- the echo row -- takes
//! intra-process deltas, so a process-local monotonic source is enough.

/// Current `CLOCK_MONOTONIC` in nanoseconds, the same clock and formula the
/// unix tap sites use.
#[cfg(unix)]
#[must_use]
pub fn monotonic_nanos() -> i64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(now.tv_nsec)
}

/// Monotonic nanoseconds from a fixed process-local epoch, for platforms
/// without the raw clock the unix tap sites read. Valid only within this
/// process, which is sufficient because the sole off-unix caller (the echo
/// row) takes intra-process deltas.
#[cfg(not(unix))]
#[must_use]
pub fn monotonic_nanos() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;

    static EPOCH: OnceLock<Instant> = OnceLock::new();
    i64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::monotonic_nanos;

    #[test]
    fn monotonic_nanos_is_monotone() {
        let a = monotonic_nanos();
        let b = monotonic_nanos();
        assert!(b >= a);
    }
}
