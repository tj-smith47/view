//! What the terminal answered about itself, and the tier that answer
//! reads as.

/// Detected terminal capabilities.
///
/// `tier` is coarse UX vocabulary; the probed bits are what gates behavior
/// (BSU/ESU gates on `caps.sync`, the border charset on
/// `caps.unicode_boxes`, never on tier alone).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCaps {
    pub tier: Tier,
    pub sync: bool,
    pub truecolor: bool,
    pub kitty_kbd: bool,
    /// Whether the terminal accounts for a box-drawing glyph as one cell,
    /// which is what the border charset asks about.
    ///
    /// A cell-accounting fact, not a legibility one: the probe behind it
    /// writes one `╭` and reads the cursor column back, so a terminal not
    /// decoding UTF-8 is what a `false` here has actually been shown to
    /// mean. A font that lacks the glyph still advances one column and
    /// renders tofu, and no capture has separated that case from a working
    /// one -- see `docs/terminal-probe-wire-capture.md`, "What D and E
    /// prove, and what they do not".
    pub unicode_boxes: bool,
    /// Whether the terminal itself answered the box-glyph question with a
    /// cursor one cell past `╭`. A `unicode_boxes` resolved from the locale
    /// hint leaves this false.
    pub boxes_measured: bool,
}

impl Default for TermCaps {
    /// Conservative defaults used before any capability probe runs: no
    /// probe is assumed to have succeeded. Routed through [`Self::from_probe`]
    /// (all-false) rather than hand-coded, so the tier-derivation formula
    /// still lives in exactly one place and a default of all-false booleans
    /// can never disagree with what `from_probe(false, false, false)` would
    /// derive for `tier`.
    fn default() -> Self {
        Self::from_probe(false, false, false)
    }
}

impl TermCaps {
    /// Builds capabilities from the three probed booleans, deriving `tier`
    /// the same way for every caller (auto-detection and the `--tier`
    /// override both funnel through this, so the derivation rule lives in
    /// exactly one place): `sync && truecolor && kitty_kbd` is `Full`,
    /// `truecolor` alone is `Standard`, anything else is `Basic`.
    ///
    /// `#[non_exhaustive]` keeps `TermCaps` from being struct-literal
    /// constructed outside this crate, but the terminal probe that
    /// discovers these booleans can only live in `view-tui` (only that
    /// crate touches the terminal), so this constructor is the sanctioned
    /// crossing point.
    #[must_use]
    pub fn from_probe(sync: bool, truecolor: bool, kitty_kbd: bool) -> Self {
        let tier = if sync && truecolor && kitty_kbd {
            Tier::Full
        } else if truecolor {
            Tier::Standard
        } else {
            Tier::Basic
        };
        Self {
            tier,
            sync,
            truecolor,
            kitty_kbd,
            unicode_boxes: false,
            boxes_measured: false,
        }
    }

    /// The same capabilities with [`Self::unicode_boxes`] set to what the
    /// box-glyph probe answered.
    ///
    /// Set beside [`Self::from_probe`] rather than through it because
    /// `tier` does not derive from it: the border charset is the one thing
    /// this bit gates, and a terminal's cell accounting is independent of
    /// the color depth, synchronization and keyboard-protocol answers a
    /// tier is made of. Left `false` by every caller that never asked the
    /// question, which is the same floor an unanswered probe resolves to.
    #[must_use]
    pub fn with_unicode_boxes(self, unicode_boxes: bool) -> Self {
        Self {
            unicode_boxes,
            ..self
        }
    }

    /// The same capabilities with [`Self::boxes_measured`] set.
    #[must_use]
    pub fn with_boxes_measured(self, boxes_measured: bool) -> Self {
        Self {
            boxes_measured,
            ..self
        }
    }
}

/// Coarse terminal capability tier.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Full,
    Standard,
    Basic,
}
