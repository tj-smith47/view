//! The OSC52 clipboard side channel: the job the executor queues, the one
//! terminal capability draining it needs, and the drain itself.
//!
//! Its own module rather than a corner of `runtime`: the escape is written
//! by the loop thread but decided nowhere near the loop, and the cap and
//! skip-and-log policy below is the whole of what "copy to the terminal's
//! clipboard" means here.

use std::sync::mpsc;

use view_core::msg::{RegisterType, OSC52_MAX_PAYLOAD_BYTES};
use view_tui::terminal::Term;

/// One OSC52 clipboard-set escape to write, queued by
/// [`crate::runtime::Executor::run`] and drained by
/// [`crate::runtime::run`]'s loop into
/// [`view_tui::terminal::Term::write_osc52`].
pub struct Osc52Job {
    pub register: char,
    pub lines: Vec<String>,
    pub regtype: RegisterType,
}

/// The one capability [`drain_osc52`] needs of the real terminal, factored
/// out of [`Term`] the same way [`crate::engine_ops::EngineOps`] is factored
/// out of `EngineHandle`: a test drives the cap-before-encode and
/// skip-and-log logic below against a recording fake, with no real terminal
/// and no stdout write in the loop.
pub(crate) trait Osc52Sink {
    fn write_osc52(&mut self, register: char, text: &str) -> std::io::Result<()>;
}

impl Osc52Sink for Term {
    fn write_osc52(&mut self, register: char, text: &str) -> std::io::Result<()> {
        Term::write_osc52(self, register, text)
    }
}

/// Drains every OSC52 job currently queued on `osc52_rx` into `sink`,
/// applying the same cap-before-encode and skip-and-log policy
/// `Effect::Osc52Copy`'s doc states: an over-cap payload is logged and
/// never handed to `sink` at all (no truncated write), and a write error
/// is logged rather than propagated, since nothing on the wire is blocked
/// on this fire-and-forget escape (see [`Osc52Sink`]'s doc for the seam
/// this drives against in tests).
pub(crate) fn drain_osc52<S: Osc52Sink>(osc52_rx: &mpsc::Receiver<Osc52Job>, sink: &mut S) {
    while let Ok(job) = osc52_rx.try_recv() {
        let text = view_native::clipboard::lines_to_text(&job.lines, job.regtype);
        // base64 expands 3 raw bytes to 4 encoded ones; this is the
        // size the terminal actually receives and the bound
        // `OSC52_MAX_PAYLOAD_BYTES` states in `Osc52Copy`'s own doc
        let encoded_len = text.len().div_ceil(3) * 4;
        if encoded_len > OSC52_MAX_PAYLOAD_BYTES {
            crate::vlog::log_with("osc52", || {
                format!(
                    "skipped a {encoded_len}-byte payload over the \
                     {OSC52_MAX_PAYLOAD_BYTES}-byte cap"
                )
            });
            continue;
        }
        // fire-and-forget per `Effect::Osc52Copy`'s doc: nothing on the
        // wire is blocked on this escape, so a transient stdout error
        // here must not tear the session down the way a frame-paint
        // failure does (`draw_surface`'s `?` in `run`, which the terminal's
        // own real content depends on)
        if let Err(err) = sink.write_osc52(job.register, &text) {
            crate::vlog::log_with("osc52", || format!("write failed: {err}"));
        }
    }
}

/// A recording [`Osc52Sink`] fake, the same shape as `runtime`'s own
/// `FakeOps`: `drain_osc52`'s cap/skip/write logic is driven against this
/// instead of a real terminal, so the 100 KiB cap and the skip-and-log path
/// are provable with no stdout write and no sleep.
///
/// Module level rather than inside the test module below because the loop's
/// own handoff test drains through it too, and one fake with one behaviour
/// is what keeps those two tests agreeing on what a sink does.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeOsc52Sink {
    pub(crate) writes: Vec<(char, String)>,
}

#[cfg(test)]
impl Osc52Sink for FakeOsc52Sink {
    fn write_osc52(&mut self, register: char, text: &str) -> std::io::Result<()> {
        self.writes.push((register, text.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// One raw byte short of the base64-expanded cap
    /// (`div_ceil(3) * 4 == OSC52_MAX_PAYLOAD_BYTES` exactly at this
    /// length): the boundary case `encoded_len > OSC52_MAX_PAYLOAD_BYTES`
    /// must not reject, since `>` (not `>=`) is the cap's own contract.
    fn at_cap_text() -> String {
        // OSC52_MAX_PAYLOAD_BYTES is a multiple of 4, so 3/4 of it is a
        // whole number of raw bytes whose base64 expansion lands exactly
        // on the cap with no remainder to round up
        "a".repeat(OSC52_MAX_PAYLOAD_BYTES / 4 * 3)
    }

    #[test]
    fn an_at_cap_payload_is_written_whole() {
        let (tx, rx) = mpsc::channel();
        let text = at_cap_text();
        let encoded_len = text.len().div_ceil(3) * 4;
        assert_eq!(
            encoded_len, OSC52_MAX_PAYLOAD_BYTES,
            "fixture must sit exactly at the cap, not merely under it"
        );
        tx.send(Osc52Job {
            register: '+',
            lines: vec![text.clone()],
            regtype: RegisterType::Charwise,
        })
        .unwrap();

        let mut sink = FakeOsc52Sink::default();
        drain_osc52(&rx, &mut sink);

        assert_eq!(sink.writes.len(), 1, "an at-cap payload must be written");
        assert_eq!(sink.writes[0].0, '+');
        assert_eq!(
            sink.writes[0].1, text,
            "the written text must be whole, not truncated"
        );
    }

    #[test]
    fn an_over_cap_payload_is_skipped_with_no_write_attempted() {
        let (tx, rx) = mpsc::channel();
        // one raw byte past `at_cap_text`'s length pushes the base64
        // expansion strictly over the cap
        let text = "a".repeat(OSC52_MAX_PAYLOAD_BYTES / 4 * 3 + 1);
        let encoded_len = text.len().div_ceil(3) * 4;
        assert!(
            encoded_len > OSC52_MAX_PAYLOAD_BYTES,
            "fixture must sit strictly over the cap"
        );
        tx.send(Osc52Job {
            register: '+',
            lines: vec![text],
            regtype: RegisterType::Charwise,
        })
        .unwrap();

        let mut sink = FakeOsc52Sink::default();
        drain_osc52(&rx, &mut sink);

        assert!(
            sink.writes.is_empty(),
            "an over-cap payload must never reach the sink, truncated or otherwise"
        );
    }
}
