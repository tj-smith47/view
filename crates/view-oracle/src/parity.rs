//! The parity relation the corpus runner drives: state probes plus a masked
//! grid diff, comparing view's own session against [`crate::ReferenceSession`]
//! (or any other [`Probe`] source) rather than against nvim's ground truth
//! directly. [`ReferenceSession`]'s own module docs already cover why a
//! second independent applier is the differential; this module is the
//! comparison layer that turns two sides' state into a list of concrete
//! disagreements.
//!
//! Three comparison axes, kept separate because they diverge for different
//! reasons and a caller needs to tell them apart:
//! - [`Divergence::State`]: the two sides' `nvim_eval` state probes
//!   ([`StateSnapshot`]) disagree -- buffer text, cursor, mode, registers,
//!   or marks. A bug in `view`'s own input handling or engine wiring, not a
//!   rendering bug.
//! - [`Divergence::Grid`]: the two sides' rendered screen *text* disagrees at
//!   a specific row not excluded by [`masked_rows`]. A rendering/apply bug
//!   in `view`'s `Model`/`Grid`/`Surface` pipeline (the exact class
//!   [`crate::ReferenceSession`]'s `RefGrid` exists to catch).
//! - [`Divergence::Attr`]: the two sides agree on a row's glyphs but not on
//!   its per-cell *highlights* -- a style/attr bug (e.g. in a frame-scoped
//!   hl cache) that leaves the text untouched. Each cell's `hl_id` is
//!   resolved to its rendered attributes before comparison, so the two
//!   sessions' independent id assignments never register as a difference
//!   (see [`crate::attr`]'s docs).
//!
//! [`masked_rows`] excludes rows [`crate::ReferenceSession`] cannot ever
//! agree on by construction: its `RefGrid` never receives
//! `Cmdline*`/`Msg*`/`Tabline*`/`Popupmenu*` content (see
//! `ReferenceSession::apply`'s own doc comment), so any row view's real
//! `Surface` paints one of those overlays into is not a real behavioral
//! disagreement, just an artifact of the reference applier's own scope.
//!
//! Registers, marks, and the cursor all ride one shared set of vimscript
//! probe expressions and one shared Rust parser ([`parse_cursor`],
//! [`parse_marks`]) across both sides of [`compare`]: a capture bug in
//! that shared expression or shared parser is common-mode between `view`'s
//! side and [`ReferenceSession`]'s side and so invisible to this module's
//! differential, which only ever proves the two sides agree, not that
//! either captured the underlying nvim state correctly in the first place
//! (the same limit [`ReferenceSession::apply`](crate::ReferenceSession)'s
//! own doc comment names for `clamp_dim`/`saturate_u16`). [`snapshot`]
//! narrows what it can: a probe reply that does not parse fails loudly as
//! an [`OracleError`] instead of degrading to a placeholder that would
//! compare equal against an identically malformed reply from the other
//! side.

use std::time::{Duration, Instant};

use view_surface::{LayerKind, Surface};

use crate::{EngineSession, OracleError, ReferenceSession};

/// A single-field separator inside one probe reply record (e.g. between a
/// mark's name and its row/column). Vim's `nr2char(31)` (ASCII unit
/// separator): a control byte no mark name, register content, or line
/// number ever legitimately contains, unlike a comma or pipe.
const FIELD_SEP: &str = "\u{1f}";
/// The separator between records in a multi-record probe reply (one mark
/// per record). Vim's `nr2char(30)` (ASCII record separator), chosen
/// alongside [`FIELD_SEP`] from the same control-byte range for the same
/// collision-freedom reason.
const RECORD_SEP: &str = "\u{1e}";

/// The fixed, printable register set every [`snapshot`] probes: unnamed
/// (`"`), the two numbered yank/delete registers `view` scripts exercise
/// most (`0`, `1`), and one named register (`a`) so a script that
/// explicitly targets a register (`"ayy`) has something to disagree on.
/// Fixed rather than "every register nvim has" because most of nvim's
/// register space (`b`-`z`, `A`-`Z`, `*`/`+`/clipboard, `:`, `.`, `%`, `#`)
/// is either untouched by any scripted scenario this oracle drives or reads
/// host clipboard/session state a hermetic probe must never depend on.
const REGISTER_NAMES: [char; 4] = ['"', '0', '1', 'a'];

/// Bound on [`snapshot`]'s wait for a blocked session to leave its
/// key-wait after the `<Esc>` dismissal (see [`snapshot`]'s own doc
/// comment): generous relative to processing a single already-queued
/// keystroke, short enough that a session `<Esc>` genuinely cannot
/// unblock fails the probe promptly as [`OracleError::Blocked`] instead
/// of hanging the run.
const UNBLOCK_DEADLINE: Duration = Duration::from_secs(5);

/// Anything a differential parity check can read state from: `view`'s own
/// [`EngineSession`] and [`ReferenceSession`] both qualify, since both wrap
/// a real embedded engine's `nvim_eval`. Kept as a trait (not a concrete
/// parameter type) so [`snapshot`] and any future probe helper work
/// identically against either side without a second copy of the parsing
/// logic -- the two sides' engines are the ground truth this whole module
/// diffs against each other, so the diffing code must not care which one it
/// is currently reading.
pub trait Probe {
    /// Evaluates `expr` and returns its result as text, identical in
    /// contract to [`EngineSession::eval_str`] and
    /// [`ReferenceSession::eval_str`] (the two implementations this trait
    /// exists to unify).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError`] under the same conditions as the underlying
    /// session's own `eval_str`.
    fn eval_str(&mut self, expr: &str) -> Result<String, OracleError>;

    /// Reads the session's current mode name and blocked flag via the fast
    /// `nvim_get_mode` probe, identical in contract to
    /// [`EngineSession::get_mode`] and [`ReferenceSession::get_mode`]:
    /// answered even in the blocked key-wait states where
    /// [`eval_str`](Self::eval_str) is deferred, which is what lets
    /// [`snapshot`] decide whether its eval probes can run at all.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError`] under the same conditions as the underlying
    /// session's own `get_mode`.
    fn get_mode(&mut self) -> Result<(String, bool), OracleError>;

    /// Forwards one encoded key `notation` via `nvim_input`, identical in
    /// contract to [`EngineSession::input`] and [`ReferenceSession::input`]:
    /// how [`snapshot`] dismisses a blocked key-wait (see its doc comment)
    /// before running probes that a blocked session would defer.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError`] under the same conditions as the underlying
    /// session's own `input`.
    fn input(&mut self, notation: &str) -> Result<(), OracleError>;
}

impl Probe for EngineSession {
    fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        EngineSession::eval_str(self, expr)
    }

    fn get_mode(&mut self) -> Result<(String, bool), OracleError> {
        EngineSession::get_mode(self)
    }

    fn input(&mut self, notation: &str) -> Result<(), OracleError> {
        EngineSession::input(self, notation)
    }
}

impl Probe for ReferenceSession {
    fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        ReferenceSession::eval_str(self, expr)
    }

    fn get_mode(&mut self) -> Result<(String, bool), OracleError> {
        ReferenceSession::get_mode(self)
    }

    fn input(&mut self, notation: &str) -> Result<(), OracleError> {
        ReferenceSession::input(self, notation)
    }
}

/// One side's probed state: buffer text, cursor, mode plus blocked flag, a
/// fixed register set, and marks. Not a full state dump (nvim has far more
/// introspectable state than this): the fields here are exactly what a
/// scripted key-notation scenario can make disagree between `view`'s own
/// engine and [`ReferenceSession`]'s -- what the buffer holds, where the
/// cursor sits, what mode nvim is in (and whether it sits blocked in a
/// key-wait), what a yank/delete left in a register, and where a mark
/// landed.
#[derive(Debug, Clone, PartialEq)]
pub struct StateSnapshot {
    pub buffer_lines: Vec<String>,
    pub cursor: (u64, u64),
    pub mode: String,
    /// Whether the session sat blocked in a key-wait (a hit-enter prompt,
    /// a pending `t`/`f`/`r` character argument) when this snapshot was
    /// taken -- captured before the `<Esc>` dismissal [`snapshot`] performs
    /// to let its eval probes answer, so the wait itself stays a compared
    /// state rather than being silently probed away.
    ///
    /// Granularity boundary: this is one flag, not an identification of
    /// *which* wait. Two different waits that report the same mode name --
    /// a pending `t` target character vs a register name after `"`, both
    /// `("n", blocking)` from the fast probe -- compare equal here even
    /// though the two sessions would consume their next key differently.
    /// That is the fast-API surface's own limit: `nvim_get_mode` (the only
    /// probe a blocked main loop answers) exposes exactly the mode name
    /// and this flag, and telling the waits apart would need a non-fast
    /// probe that a blocked session defers -- the wedge this field exists
    /// to avoid.
    pub blocked: bool,
    pub registers: Vec<(char, String)>,
    pub marks: Vec<(String, u64, u64)>,
}

/// Probes `probe` for a full [`StateSnapshot`]: the fast `nvim_get_mode`
/// probe for the mode name and blocked flag, then one `nvim_eval` round
/// trip for the buffer, one for the cursor, one per [`REGISTER_NAMES`]
/// entry, and two for marks (buffer-local, then global -- see
/// [`marks_expr`]).
///
/// The mode comes from `nvim_get_mode` (which reports it in `mode(1)`'s own
/// format) rather than an eval probe, and it is read first, because the two
/// probe kinds have different availability: nvim defers every non-fast
/// request -- all the eval probes below -- while its main loop is blocked
/// waiting for a key (a hit-enter prompt, a pending `t`/`f`/`r` character
/// argument, a register name after `"`), but answers `nvim_get_mode`
/// immediately in exactly those states. A script is free to end blocked, and
/// that wait is real compared state ([`StateSnapshot::blocked`]), so a
/// blocked session must neither wedge the probe run (the eval-timeout
/// failure seeded fuzz once quarantined as `fuzz-42-6`) nor be skipped: the
/// pre-dismissal mode/blocked pair is captured, then the wait is dismissed
/// with a single `<Esc>` -- which aborts a pending key-wait without touching
/// buffer text, cursor, registers, or marks (live-verified against the
/// pinned nvim) -- and once the fast probe confirms the session unblocked,
/// the eval probes below read the rest of the state the script actually
/// produced. Each side of a differential is handled identically by
/// construction (this one shared function), so a side that blocks when the
/// other does not still surfaces as a `blocked` state divergence rather
/// than disappearing into asymmetric handling.
///
/// Every list-shaped probe (buffer lines, marks) is
/// joined vimscript-side with [`FIELD_SEP`]/[`RECORD_SEP`] (or, for buffer
/// lines, a literal newline -- no nvim buffer line can ever contain an
/// embedded `\n`, since that byte is exactly what separates lines in nvim's
/// own model) rather than shipped as a raw `nvim_eval` array/dict result:
/// `EngineHandle::eval_str` renders a structured `Value` through `rmpv`'s
/// own `Display`, a format this crate's dependency-direction audit
/// (`scripts/audit-deps.sh`) forbids parsing here (no `serde`/`serde_json`
/// allowed in `view-oracle`), so the join happens on the nvim side instead,
/// in a delimiter this code controls end to end.
///
/// # Errors
///
/// Returns [`OracleError`] if any of the underlying `get_mode`/`eval_str`
/// calls fail, if a blocked session stays blocked past [`UNBLOCK_DEADLINE`]
/// after the `<Esc>` dismissal ([`OracleError::Blocked`], naming the mode),
/// or if a cursor/marks reply does not parse (see [`parse_cursor`],
/// [`parse_marks`]) -- a malformed reply is a loud probe failure, not a
/// value degraded to a placeholder that could compare equal against an
/// identically malformed reply from the other side.
pub fn snapshot(probe: &mut dyn Probe) -> Result<StateSnapshot, OracleError> {
    snapshot_with_deadline(probe, UNBLOCK_DEADLINE)
}

/// [`snapshot`] with the blocked-wait dismissal bound injectable: the
/// loud-failure arm (a session that never unblocks) can only be exercised
/// by actually reaching the deadline, so proving it must not cost a full
/// [`UNBLOCK_DEADLINE`] wait every run. [`snapshot`] is the sole
/// production caller and always passes the const.
fn snapshot_with_deadline(
    probe: &mut dyn Probe,
    unblock_deadline: Duration,
) -> Result<StateSnapshot, OracleError> {
    let (mode, blocked) = probe.get_mode()?;
    if blocked {
        dismiss_blocked_wait(probe, unblock_deadline)?;
    }

    let buffer_lines = probe
        .eval_str("join(getline(1,'$'), \"\\n\")")?
        .split('\n')
        .map(str::to_string)
        .collect();

    let cursor_raw = probe.eval_str(&format!("join(getpos('.'), \"{FIELD_SEP}\")"))?;
    let cursor = parse_cursor(&cursor_raw)?;

    let mut registers = Vec::with_capacity(REGISTER_NAMES.len());
    for name in REGISTER_NAMES {
        let value = probe.eval_str(&format!("getreg('{name}')"))?;
        registers.push((name, value));
    }

    let local_marks_raw = probe.eval_str(&marks_expr("bufnr('%')"))?;
    let mut marks = parse_marks(&local_marks_raw)?;
    let global_marks_raw = probe.eval_str(&marks_expr(""))?;
    marks.extend(parse_marks(&global_marks_raw)?);

    Ok(StateSnapshot {
        buffer_lines,
        cursor,
        mode,
        blocked,
        registers,
        marks,
    })
}

/// Types a single `<Esc>` to abort the blocked key-wait `probe`'s fast
/// mode probe just reported, then re-polls that same fast probe until the
/// session leaves the blocked state, bounded by `deadline`. See
/// [`snapshot`]'s doc comment for why `<Esc>` is a state-preserving
/// dismissal and why the pre-dismissal mode/blocked pair is what the
/// snapshot itself carries.
///
/// # Errors
///
/// Returns [`OracleError::Blocked`] (naming the still-blocked mode) if the
/// session has not unblocked by the deadline, or the underlying
/// `input`/`get_mode` error if either call fails outright.
fn dismiss_blocked_wait(probe: &mut dyn Probe, deadline: Duration) -> Result<(), OracleError> {
    probe.input("<Esc>")?;
    let start = Instant::now();
    loop {
        let (mode, blocked) = probe.get_mode()?;
        if !blocked {
            return Ok(());
        }
        if start.elapsed() >= deadline {
            return Err(OracleError::Blocked { mode });
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Builds a `getmarklist(...)` probe expression, `buf_arg` either
/// `"bufnr('%')"` (buffer-local marks: `a`-`z`, `'`, `"`, `[`, `]`, `.`, `^`)
/// or `""` (global marks: `A`-`Z`, `0`-`9`) -- per `:help getmarklist()`,
/// the no-argument form returns the global list, not an empty/duplicate
/// view of the buffer-local one. Live-verified against the pinned nvim: a
/// mark set with `mA` shows up in the no-arg form and nowhere in the
/// buffer-arg form, confirming the two calls are genuinely disjoint rather
/// than redundant.
fn marks_expr(buf_arg: &str) -> String {
    format!(
        "join(map(getmarklist({buf_arg}), 'v:val.mark . \"{FIELD_SEP}\" . v:val.pos[1] . \"{FIELD_SEP}\" . v:val.pos[2]'), \"{RECORD_SEP}\")"
    )
}

/// Parses a `join(getpos('.'), FIELD_SEP)` reply (`bufnum, lnum, col, off`)
/// into `(lnum, col)`: [`StateSnapshot::cursor`] only ever needs the
/// position, not which buffer or the virtualedit offset.
///
/// # Errors
///
/// Returns [`OracleError::Parse`] if `raw` does not split into at least
/// three [`FIELD_SEP`]-delimited numeric fields. Fails loudly rather than
/// degrading to `(0, 0)`: both sides of [`compare`] share this expression
/// and this parser (see this module's own doc comment), so a silent
/// placeholder here would let a malformed reply on both sides compare
/// equal and vanish instead of surfacing as a broken probe.
fn parse_cursor(raw: &str) -> Result<(u64, u64), OracleError> {
    let mut fields = raw.split(FIELD_SEP);
    let _bufnum = fields.next();
    let malformed = || OracleError::Parse(format!("malformed getpos reply: {raw:?}"));
    let line = fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(malformed)?;
    let col = fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(malformed)?;
    Ok((line, col))
}

/// Parses a `join(map(getmarklist(...), ...), RECORD_SEP)` reply into
/// `(mark name, row, col)` triples. An empty reply (no marks at all, which
/// cannot happen once nvim has set its own automatic marks but is the
/// correct degenerate case) yields an empty `Vec` rather than a one-element
/// `Vec` holding an empty record: `"".split(RECORD_SEP)` would otherwise
/// yield one empty string, which the inner field split below would then
/// fail to parse -- checking `is_empty()` up front says the degenerate case
/// is expected instead of routing it through the same path a genuine
/// parse failure takes.
///
/// # Errors
///
/// Returns [`OracleError::Parse`] on the first record that does not split
/// into a name plus two [`FIELD_SEP`]-delimited numeric fields. Fails
/// loudly rather than `filter_map`-ing the bad record away: silently
/// dropping one record out of a shared probe/parser both sides of
/// [`compare`] ride (see this module's own doc comment) is exactly the
/// kind of common-mode coverage loss a differential oracle must not hide.
fn parse_marks(raw: &str) -> Result<Vec<(String, u64, u64)>, OracleError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    raw.split(RECORD_SEP)
        .map(|record| {
            let malformed =
                || OracleError::Parse(format!("malformed getmarklist record: {record:?}"));
            let mut fields = record.split(FIELD_SEP);
            let name = fields.next().ok_or_else(malformed)?.to_string();
            let row = fields
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(malformed)?;
            let col = fields
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(malformed)?;
            Ok((name, row, col))
        })
        .collect()
}

/// One concrete disagreement between `view`'s side and the reference side,
/// found by [`compare`]. Three arms because a state-probe disagreement, a
/// rendered-glyph disagreement, and a rendered-attribute disagreement point
/// at different layers of the stack (see this module's own doc comment) and a
/// caller triaging a failure needs to know immediately which kind it is
/// looking at, not just that the two sides disagreed somewhere.
#[derive(Debug)]
pub enum Divergence {
    /// A [`StateSnapshot`] field disagreed; `field` names which one
    /// (`"buffer_lines"`, `"cursor"`, `"mode"`, `"blocked"`, `"registers"`,
    /// or `"marks"`).
    State {
        field: String,
        view: String,
        reference: String,
    },
    /// Rendered glyph row `row` disagreed and was not in the caller's mask.
    Grid {
        row: u16,
        view: String,
        reference: String,
    },
    /// Rendered highlight row `row` disagreed and was not in the caller's
    /// mask: the two sides painted the same (or a masked-away) glyph but a
    /// cell's resolved attributes differ. `view`/`reference` carry the two
    /// sides' [`crate::attr::ResolvedAttr`] row fingerprints, so a report
    /// names what differed rather than only that something did -- a
    /// glyph-equal-but-styled-differently row is exactly the class the
    /// text-only [`Divergence::Grid`] diff cannot see.
    Attr {
        row: u16,
        view: String,
        reference: String,
    },
}

/// [`Divergence`]'s variant, stripped of its payload: the granularity a
/// minimizer's reproduction predicate needs, since two
/// [`compare`]-produced divergence lists count as "the same failure" when
/// their first entries share this tag, regardless of which buffer line or
/// grid row the payload happens to name (a minimized script's exact row
/// index or buffer content is expected to shift as tokens drop out; the
/// variant it fails on is not). `Grid` and `Attr` are kept distinct here so
/// a minimizer never reduces a text-render divergence toward an unrelated
/// attr-render one, or the reverse -- they are different rendering bugs even
/// when they land on the same row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceKind {
    State,
    Grid,
    Attr,
}

impl Divergence {
    /// This divergence's [`DivergenceKind`], discarding the field/row and
    /// view/reference payload.
    #[must_use]
    pub fn kind(&self) -> DivergenceKind {
        match self {
            Self::State { .. } => DivergenceKind::State,
            Self::Grid { .. } => DivergenceKind::Grid,
            Self::Attr { .. } => DivergenceKind::Attr,
        }
    }
}

/// One side's rendered screen: the glyph dump plus the per-cell highlight
/// dump, held together because [`compare`] diffs both against the other
/// side's same pair and a caller must never diff one side's glyphs against a
/// stale frame's attributes. `rows` and `attr_rows` are row-indexed
/// identically (same chrome offset), so a given index names the same on-screen
/// row in both -- which is what lets one shared [`masked_rows`] mask suppress
/// overlay rows in both diffs at once.
#[derive(Debug, Clone)]
pub struct Screen {
    /// One glyph string per canvas row (from `screen_rows`).
    pub rows: Vec<String>,
    /// One [`crate::attr::ResolvedAttr`] fingerprint string per canvas row
    /// (from `attr_rows`), aligned cell-for-cell with `rows`.
    pub attr_rows: Vec<String>,
}

/// Diffs `view_state` against `ref_state` field by field, then the two
/// screens' glyph rows, then their attribute rows, skipping any row index
/// present in `mask` (see [`masked_rows`]) -- the ordering is arbitrary and
/// not load-bearing; every field/row that disagrees produces its own
/// [`Divergence`], so this always returns the *complete* set of disagreements
/// between the two sides, not just the first.
///
/// Glyph and attribute rows are diffed as two independent passes producing
/// [`Divergence::Grid`] and [`Divergence::Attr`] respectively: a row whose
/// text matches but whose highlights differ (a style/attr bug that leaves the
/// glyphs untouched) surfaces as an `Attr` divergence naming the row and the
/// two resolved-attr renderings, never vanishing into a text-only diff that
/// sees the glyphs agree.
///
/// A row present on only one side of either pair (the two sides disagreeing
/// on total row count) is treated as disagreeing against an empty string on
/// the shorter side, rather than being silently skipped: a row count mismatch
/// is itself real signal a differential oracle must surface.
#[must_use]
pub fn compare(
    view_state: &StateSnapshot,
    ref_state: &StateSnapshot,
    view: &Screen,
    reference: &Screen,
    mask: &[u16],
) -> Vec<Divergence> {
    let mut divergences = Vec::new();

    if view_state.buffer_lines != ref_state.buffer_lines {
        divergences.push(Divergence::State {
            field: "buffer_lines".to_string(),
            view: view_state.buffer_lines.join("\n"),
            reference: ref_state.buffer_lines.join("\n"),
        });
    }
    if view_state.cursor != ref_state.cursor {
        divergences.push(Divergence::State {
            field: "cursor".to_string(),
            view: format!("{:?}", view_state.cursor),
            reference: format!("{:?}", ref_state.cursor),
        });
    }
    if view_state.mode != ref_state.mode {
        divergences.push(Divergence::State {
            field: "mode".to_string(),
            view: view_state.mode.clone(),
            reference: ref_state.mode.clone(),
        });
    }
    if view_state.blocked != ref_state.blocked {
        divergences.push(Divergence::State {
            field: "blocked".to_string(),
            view: view_state.blocked.to_string(),
            reference: ref_state.blocked.to_string(),
        });
    }
    if view_state.registers != ref_state.registers {
        divergences.push(Divergence::State {
            field: "registers".to_string(),
            view: format!("{:?}", view_state.registers),
            reference: format!("{:?}", ref_state.registers),
        });
    }
    if view_state.marks != ref_state.marks {
        divergences.push(Divergence::State {
            field: "marks".to_string(),
            view: format!("{:?}", view_state.marks),
            reference: format!("{:?}", ref_state.marks),
        });
    }

    diff_rows(
        &view.rows,
        &reference.rows,
        mask,
        &mut divergences,
        |row, v, r| Divergence::Grid {
            row,
            view: v.to_string(),
            reference: r.to_string(),
        },
    );
    diff_rows(
        &view.attr_rows,
        &reference.attr_rows,
        mask,
        &mut divergences,
        |row, v, r| Divergence::Attr {
            row,
            view: v.to_string(),
            reference: r.to_string(),
        },
    );

    divergences
}

/// Diffs two row vectors index by index, skipping masked rows and pushing a
/// `make`-built [`Divergence`] for each unmasked disagreement. Shared by
/// [`compare`]'s glyph pass and attr pass so both handle a row-count
/// mismatch (diffing against an empty string on the shorter side) and the
/// mask identically -- the only thing that differs between the two passes is
/// which [`Divergence`] variant a disagreement becomes.
fn diff_rows(
    view_rows: &[String],
    ref_rows: &[String],
    mask: &[u16],
    out: &mut Vec<Divergence>,
    make: impl Fn(u16, &str, &str) -> Divergence,
) {
    let row_count = view_rows.len().max(ref_rows.len());
    for index in 0..row_count {
        // Unreachable in practice: both sides' canvases are u16-bounded
        // (view_surface::Rect/Grid dimensions), so row_count never exceeds
        // u16::MAX. Skip rather than panic if that invariant is ever
        // violated, since this loop is the honesty-critical comparison path.
        let Ok(row) = u16::try_from(index) else {
            continue;
        };
        if mask.contains(&row) {
            continue;
        }
        let view = view_rows.get(index).map_or("", String::as_str);
        let reference = ref_rows.get(index).map_or("", String::as_str);
        if view != reference {
            out.push(make(row, view, reference));
        }
    }
}

/// The row indices `surface`'s own overlay layers occupy: every
/// [`view_surface::Layer`] whose [`LayerKind`] is not [`LayerKind::EngineGrid`]
/// (the tabline row when showing, the cmdline row when active, the toast
/// box's rows when a message is visible, the shell placeholder before first
/// paint, the popup menu while open). [`ReferenceSession`]'s `RefGrid` never
/// receives any of that content (see this module's own doc comment), so
/// every row one of these layers claims is a row the two sides can never
/// agree on by construction, not a real behavioral disagreement.
///
/// Takes `surface` directly, not a session, so the exact [`Surface`] value a
/// caller's [`compare`] call reads its `view_rows` from is the same one this
/// mask is computed from: one `render()`/`surface()` call feeds both, so the
/// mask can never silently drift out of sync with what the row text actually
/// contains a frame later.
#[must_use]
pub fn masked_rows(surface: &Surface) -> Vec<u16> {
    let mut rows: Vec<u16> = surface
        .layers
        .iter()
        .filter(|layer| !matches!(layer.kind, LayerKind::EngineGrid))
        .flat_map(|layer| {
            let start = layer.rect.row;
            let end = start.saturating_add(layer.rect.height);
            start..end
        })
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::time::Duration;
    use view_core::events::UiEvent;
    use view_core::grid::GridOp;
    use view_core::model::Model;
    use view_core::msg::Msg;
    use view_core::update::update;

    use crate::testenv;

    fn snapshot_fixture() -> StateSnapshot {
        StateSnapshot {
            buffer_lines: vec!["hello".to_string(), "world".to_string()],
            cursor: (1, 0),
            mode: "n".to_string(),
            blocked: false,
            registers: vec![('"', "hello\n".to_string()), ('0', "hello\n".to_string())],
            marks: vec![("'.".to_string(), 1, 0)],
        }
    }

    fn rows_fixture() -> Vec<String> {
        vec!["hello     ".to_string(), "world     ".to_string()]
    }

    /// A [`Screen`] with the given glyph rows and empty attr rows: the glyph
    /// and state passes are what most of these tests exercise, so leaving
    /// `attr_rows` empty on both sides keeps the attr pass silent (an empty
    /// vs empty diff produces nothing) and lets a test isolate the pass it
    /// means to. Attr-pass behavior is proven by the dedicated attr tests
    /// below, which set `attr_rows` explicitly.
    fn screen(rows: Vec<String>) -> Screen {
        Screen {
            rows,
            attr_rows: Vec::new(),
        }
    }

    /// The falsifiable check this whole module exists for, arm 1: a doctored
    /// register (everything else identical) must produce exactly one
    /// `Divergence::State` naming the `"registers"` field, and nothing else
    /// -- not a `Grid` divergence, not more than one entry.
    #[test]
    fn doctored_register_produces_exactly_one_state_divergence() {
        let view_state = snapshot_fixture();
        let mut ref_state = snapshot_fixture();
        ref_state.registers[0].1 = "DOCTORED".to_string();
        let rows = rows_fixture();

        let divergences = compare(
            &view_state,
            &ref_state,
            &screen(rows.clone()),
            &screen(rows),
            &[],
        );

        assert_eq!(divergences.len(), 1, "divergences: {divergences:?}");
        match &divergences[0] {
            Divergence::State { field, .. } => assert_eq!(field, "registers"),
            other => unreachable!("expected Divergence::State, got {other:?}"),
        }
    }

    /// A doctored `blocked` flag (everything else identical) must produce
    /// exactly one `Divergence::State` naming the `"blocked"` field: one
    /// side sitting in a key-wait the other side left is a real behavioral
    /// disagreement, not probe noise to be absorbed.
    #[test]
    fn doctored_blocked_flag_produces_exactly_one_state_divergence() {
        let view_state = snapshot_fixture();
        let mut ref_state = snapshot_fixture();
        ref_state.blocked = true;
        let rows = rows_fixture();

        let divergences = compare(
            &view_state,
            &ref_state,
            &screen(rows.clone()),
            &screen(rows),
            &[],
        );

        assert_eq!(divergences.len(), 1, "divergences: {divergences:?}");
        match &divergences[0] {
            Divergence::State {
                field,
                view,
                reference,
            } => {
                assert_eq!(field, "blocked");
                assert_eq!(view, "false");
                assert_eq!(reference, "true");
            }
            other => unreachable!("expected Divergence::State, got {other:?}"),
        }
    }

    /// A [`Probe`] whose fast mode probe always reports a blocked key-wait,
    /// no matter how many `<Esc>` dismissals it receives: the hermetic
    /// stand-in for a session whose wait genuinely cannot be dismissed.
    /// Its `eval_str` fails rather than answering, because [`snapshot`]'s
    /// contract is that no eval probe runs while the session still reports
    /// blocked -- an eval reaching this probe would mean the dismissal
    /// loop leaked past its own deadline check.
    struct AlwaysBlockedProbe {
        inputs: Vec<String>,
    }

    impl Probe for AlwaysBlockedProbe {
        fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
            Err(OracleError::Parse(format!(
                "eval probe {expr:?} reached a session that never unblocked"
            )))
        }

        fn get_mode(&mut self) -> Result<(String, bool), OracleError> {
            Ok(("n".to_string(), true))
        }

        fn input(&mut self, notation: &str) -> Result<(), OracleError> {
            self.inputs.push(notation.to_string());
            Ok(())
        }
    }

    /// The loud-failure arm of the blocked-wait handling: a session that
    /// stays blocked past the dismissal deadline after the `<Esc>` must
    /// surface as [`OracleError::Blocked`] naming the still-blocked mode,
    /// never hang and never fall through to the eval probes. Also pins the
    /// dismissal itself: exactly one `<Esc>` is typed, not a retry storm.
    /// Runs through [`snapshot_with_deadline`] with a short deadline: the
    /// same code path [`snapshot`] takes, genuinely reaching the deadline
    /// rather than waiting out the production bound.
    #[test]
    fn snapshot_fails_as_blocked_when_the_dismissal_never_unblocks() {
        let mut probe = AlwaysBlockedProbe { inputs: Vec::new() };

        match snapshot_with_deadline(&mut probe, Duration::from_millis(50)) {
            Err(OracleError::Blocked { mode }) => assert_eq!(mode, "n"),
            other => unreachable!("expected OracleError::Blocked, got {other:?}"),
        }
        assert_eq!(
            probe.inputs,
            vec!["<Esc>".to_string()],
            "the dismissal path must type exactly one <Esc>"
        );
    }

    /// Arm 2: a corrupted, unmasked grid row (state identical) must produce
    /// exactly one `Divergence::Grid` naming that row.
    #[test]
    fn corrupted_grid_row_produces_exactly_one_grid_divergence() {
        let state = snapshot_fixture();
        let view_rows = rows_fixture();
        let mut ref_rows = rows_fixture();
        ref_rows[1] = "CORRUPTED ".to_string();

        let divergences = compare(&state, &state, &screen(view_rows), &screen(ref_rows), &[]);

        assert_eq!(divergences.len(), 1, "divergences: {divergences:?}");
        match &divergences[0] {
            Divergence::Grid { row, .. } => assert_eq!(*row, 1),
            other => unreachable!("expected Divergence::Grid, got {other:?}"),
        }
    }

    /// Arm 3: the same corruption as above, but at a row index present in
    /// `mask`, must produce nothing -- proving the mask actually suppresses
    /// the row, not just that an unmasked run happens to pass.
    #[test]
    fn corrupted_masked_row_produces_no_divergence() {
        let state = snapshot_fixture();
        let view_rows = rows_fixture();
        let mut ref_rows = rows_fixture();
        ref_rows[1] = "CORRUPTED ".to_string();

        let divergences = compare(&state, &state, &screen(view_rows), &screen(ref_rows), &[1]);

        assert!(
            divergences.is_empty(),
            "masked row still produced a divergence: {divergences:?}"
        );
    }

    /// [`Divergence::kind`] is the granularity a minimizer's reproduction
    /// predicate reads: a `State` divergence must report `DivergenceKind::State`
    /// regardless of which field it names, and a `Grid` divergence must
    /// report `DivergenceKind::Grid` regardless of which row -- the payload
    /// is exactly what `kind` discards.
    #[test]
    fn kind_reports_the_variant_not_the_payload() {
        let state_divergence = Divergence::State {
            field: "cursor".to_string(),
            view: "(1, 0)".to_string(),
            reference: "(2, 0)".to_string(),
        };
        let grid_divergence = Divergence::Grid {
            row: 3,
            view: "a".to_string(),
            reference: "b".to_string(),
        };
        let attr_divergence = Divergence::Attr {
            row: 5,
            view: "[b]".to_string(),
            reference: ".".to_string(),
        };
        assert_eq!(state_divergence.kind(), DivergenceKind::State);
        assert_eq!(grid_divergence.kind(), DivergenceKind::Grid);
        assert_eq!(attr_divergence.kind(), DivergenceKind::Attr);
    }

    /// Arm 4: identical state and rows on both sides, no mask, must produce
    /// nothing -- the non-tautology baseline the other three arms are
    /// deviations from.
    #[test]
    fn identical_inputs_produce_no_divergence() {
        let state = snapshot_fixture();
        let rows = rows_fixture();

        let divergences = compare(&state, &state, &screen(rows.clone()), &screen(rows), &[]);

        assert!(
            divergences.is_empty(),
            "identical inputs produced a divergence: {divergences:?}"
        );
    }

    /// The whole point of the attr pass: two sides whose glyphs agree row
    /// for row but whose resolved highlights differ on one row must surface
    /// as exactly one `Divergence::Attr` naming that row -- never a `Grid`
    /// divergence (the text is equal) and never nothing (a text-only diff's
    /// blind spot, which is the coverage gap this pass closes).
    #[test]
    fn attr_row_divergence_surfaces_when_glyphs_agree() {
        let state = snapshot_fixture();
        let rows = rows_fixture();
        let view = Screen {
            rows: rows.clone(),
            attr_rows: vec![".....".to_string(), "[b]..".to_string()],
        };
        let reference = Screen {
            rows,
            attr_rows: vec![".....".to_string(), ".....".to_string()],
        };

        let divergences = compare(&state, &state, &view, &reference, &[]);

        assert_eq!(divergences.len(), 1, "divergences: {divergences:?}");
        match &divergences[0] {
            Divergence::Attr {
                row,
                view,
                reference,
            } => {
                assert_eq!(*row, 1);
                assert_eq!(view, "[b]..");
                assert_eq!(reference, ".....");
            }
            other => unreachable!("expected Divergence::Attr, got {other:?}"),
        }
    }

    /// A masked row's attr disagreement must be suppressed by the same mask
    /// the glyph pass reads, proving the attr pass honors it rather than
    /// diffing overlay rows the reference side can never populate.
    #[test]
    fn masked_attr_row_produces_no_divergence() {
        let state = snapshot_fixture();
        let rows = rows_fixture();
        let view = Screen {
            rows: rows.clone(),
            attr_rows: vec![".....".to_string(), "[b]..".to_string()],
        };
        let reference = Screen {
            rows,
            attr_rows: vec![".....".to_string(), ".....".to_string()],
        };

        let divergences = compare(&state, &state, &view, &reference, &[1]);

        assert!(
            divergences.is_empty(),
            "masked attr row still produced a divergence: {divergences:?}"
        );
    }

    /// A row present on one side only (a row-count mismatch) must still
    /// surface as a `Grid` divergence rather than being silently skipped.
    #[test]
    fn row_count_mismatch_diverges_against_an_empty_string() {
        let state = snapshot_fixture();
        let view_rows = rows_fixture();
        let ref_rows = vec![view_rows[0].clone()];

        let divergences = compare(&state, &state, &screen(view_rows), &screen(ref_rows), &[]);

        assert_eq!(divergences.len(), 1, "divergences: {divergences:?}");
        match &divergences[0] {
            Divergence::Grid { row, reference, .. } => {
                assert_eq!(*row, 1);
                assert_eq!(reference, "");
            }
            other => unreachable!("expected Divergence::Grid, got {other:?}"),
        }
    }

    /// Pins `parse_cursor`'s expected reply shape (`bufnum, lnum, col, off`)
    /// against a literal fixture: a live-nvim disconfirm for the parser
    /// itself lives in `parse_cursor_and_parse_marks_match_a_live_getpos_
    /// and_getmarklist_reply` below; this test is the fast, hermetic pin of
    /// the parsing logic once that shape is known.
    #[test]
    fn parse_cursor_reads_lnum_and_col_from_a_getpos_shaped_reply() {
        let raw = ["0", "3", "5", "0"].join(FIELD_SEP);
        assert_eq!(parse_cursor(&raw).unwrap(), (3, 5));
    }

    /// A malformed `getpos` reply must fail loudly through `Err`, not
    /// degrade to a placeholder tuple that could compare equal against an
    /// identically malformed reply on the other side of a differential.
    #[test]
    fn parse_cursor_fails_loudly_on_a_malformed_reply() {
        assert!(matches!(parse_cursor(""), Err(OracleError::Parse(_))));
        assert!(matches!(
            parse_cursor("not-a-number"),
            Err(OracleError::Parse(_))
        ));
    }

    /// Pins `parse_marks`'s expected reply shape: `RECORD_SEP`-joined
    /// records, each `mark<FIELD_SEP>lnum<FIELD_SEP>col`.
    #[test]
    fn parse_marks_reads_name_row_and_col_from_a_getmarklist_shaped_reply() {
        let raw = [
            ["'a", "2", "3"].join(FIELD_SEP),
            ["'.", "1", "1"].join(FIELD_SEP),
        ]
        .join(RECORD_SEP);

        let marks = parse_marks(&raw).unwrap();

        assert_eq!(
            marks,
            vec![("'a".to_string(), 2, 3), ("'.".to_string(), 1, 1),]
        );
    }

    #[test]
    fn parse_marks_reads_an_empty_reply_as_no_marks() {
        assert!(parse_marks("").unwrap().is_empty());
    }

    /// One malformed record inside an otherwise well-formed reply must fail
    /// the whole probe through `Err`, not `filter_map` the bad record away
    /// and return the good ones -- a partial-but-silent drop is exactly the
    /// coverage loss a shared probe/parser must not hide from a
    /// differential.
    #[test]
    fn parse_marks_fails_loudly_on_a_malformed_record() {
        let raw = [
            ["'a", "2", "3"].join(FIELD_SEP),
            "not-enough-fields".to_string(),
        ]
        .join(RECORD_SEP);

        assert!(matches!(parse_marks(&raw), Err(OracleError::Parse(_))));
    }

    fn model_with_grid(width: u16, height: u16) -> Model {
        let mut model = Model::new();
        model.engine.apply_grid(GridOp::Resize { width, height });
        model
    }

    fn apply(model: &mut Model, ev: UiEvent) {
        let _ = update(model, Msg::Redraw(vec![ev]));
    }

    /// `masked_rows` on a plain grid-only `Surface` (no chrome active) must
    /// mask nothing: the `EngineGrid` layer itself is never masked.
    #[test]
    fn masked_rows_is_empty_with_no_chrome_active() {
        let model = model_with_grid(20, 5);
        let surface = view_surface::render(&model);

        assert!(masked_rows(&surface).is_empty());
    }

    /// A disconfirming pair for the cmdline row: an active `CmdlineShow`
    /// must mask exactly the bottom grid row. Without filtering out
    /// non-`EngineGrid` layers, this would mask nothing (empty result);
    /// masking every layer including `EngineGrid` would instead mask every
    /// row -- either wrong shape is distinguishable from the correct
    /// single-row result this test asserts.
    #[test]
    fn masked_rows_covers_the_cmdline_row_when_active() {
        let mut model = model_with_grid(20, 5);
        apply(
            &mut model,
            UiEvent::CmdlineShow {
                content: vec![(0, "wq".to_string())],
                pos: 2,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
        );
        let surface = view_surface::render(&model);

        assert_eq!(masked_rows(&surface), vec![4]);
    }

    const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
    const QUIESCE_DEADLINE: Duration = Duration::from_secs(5);

    /// End-to-end parity test: the same script driven into a real
    /// `EngineSession` and a real `ReferenceSession`, with both sides'
    /// `snapshot()`/rendered rows fed through `compare()`, must produce zero
    /// divergences. `yy` then `p` (yank-then-paste) is chosen specifically
    /// to exercise the register probes `snapshot` reads, not just the grid.
    #[test]
    fn engine_and_reference_sessions_agree_across_the_full_parity_check() {
        let mut engine_side = testenv::spawning(|| EngineSession::spawn(40, 10))
            .expect("EngineSession::spawn against real nvim");
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(40, 10))
            .expect("ReferenceSession::spawn against real nvim");

        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        engine_side
            .input("ihello<Esc>yyp")
            .expect("input against EngineSession");
        reference_side
            .input("ihello<Esc>yyp")
            .expect("input against ReferenceSession");

        assert!(engine_side.pump_until_flush(QUIESCE_DEADLINE));
        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        let view_surface = engine_side.surface();
        let view_screen = engine_side.screen();
        let mask = masked_rows(&view_surface);
        let ref_screen = reference_side.screen();

        // This scenario never opens the cmdline, a message, or
        // any other chrome layer, so the mask must be empty. An over-masking
        // regression (e.g. a `Shell` layer left painted post-attach) would
        // otherwise silently empty out every row `compare` ever looks at,
        // passing this test for the wrong reason.
        assert!(
            mask.is_empty(),
            "expected no masked rows with no chrome active, got {mask:?}"
        );

        let view_state = snapshot(&mut engine_side).expect("snapshot EngineSession");
        let ref_state = snapshot(&mut reference_side).expect("snapshot ReferenceSession");

        let divergences = compare(&view_state, &ref_state, &view_screen, &ref_screen, &mask);

        assert!(
            divergences.is_empty(),
            "engine/reference parity check found divergences: {divergences:?}\n\
             view state: {view_state:?}\nref state: {ref_state:?}\n\
             view screen: {view_screen:?}\nref screen: {ref_screen:?}\nmask: {mask:?}"
        );
        assert!(
            view_state
                .buffer_lines
                .iter()
                .filter(|l| l.trim() == "hello")
                .count()
                >= 2,
            "yy+p should have left at least two 'hello' lines: {:?}",
            view_state.buffer_lines
        );
        // `compare`'s empty result already proves the two sides
        // agree, but agreement transfers non-triviality to the reference
        // side rather than proving it -- a probe that always returns the
        // same placeholder on both sides would compare empty too. These
        // pin the actual post-`ihello<Esc>yyp` state on the view side
        // against values captured live against the pinned nvim: cursor on
        // line 2 column 1 (`p` lands the cursor on the pasted line),
        // normal mode, the unnamed register holding the linewise yank,
        // and at least one mark set by the yank/paste.
        assert_eq!(
            view_state.cursor,
            (2, 1),
            "expected cursor on the pasted line after yy+p: {:?}",
            view_state.cursor
        );
        assert!(
            view_state
                .registers
                .iter()
                .any(|(name, value)| *name == '"' && value == "hello\n"),
            "expected the unnamed register to hold the linewise yank: {:?}",
            view_state.registers
        );
        assert!(
            !view_state.marks.is_empty(),
            "expected yy+p to have set at least one mark"
        );
        assert_eq!(
            view_state.mode, "n",
            "expected normal mode after <Esc>: {:?}",
            view_state.mode
        );
    }

    /// Regression pin for the `view-harness` `tab-cycle` corpus entry: a
    /// second tab crosses `Model::chrome_rows`' one-tab threshold, which
    /// production resolves with an `nvim_ui_try_resize` down by one row to
    /// make room for the tabline (see `TablineUpdate`'s handling in
    /// `view_core::update::update`). Both `EngineSession` (via
    /// `pump_until_flush`'s effect forwarding) and `ReferenceSession` (via
    /// its own `chrome_rows`/`TablineUpdate` handling) must carry out that
    /// same resize against their own nvim process, and `ReferenceSession`
    /// must reserve a matching placeholder row in `screen_rows`, or the two
    /// sides' rows desync the moment the tabline appears -- caught live
    /// once, before this test existed, as a three-row `Divergence::Grid`
    /// mismatch when either half of that plumbing was missing.
    #[test]
    fn engine_and_reference_sessions_agree_when_a_second_tab_opens() {
        let mut engine_side = testenv::spawning(|| EngineSession::spawn(60, 12))
            .expect("EngineSession::spawn against real nvim");
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");

        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        engine_side
            .input(":tabnew<CR>gt")
            .expect("input against EngineSession");
        reference_side
            .input(":tabnew<CR>gt")
            .expect("input against ReferenceSession");

        assert!(engine_side.pump_until_flush(QUIESCE_DEADLINE));
        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        let view_surface = engine_side.surface();
        let view_screen = engine_side.screen();
        let mask = masked_rows(&view_surface);
        let ref_screen = reference_side.screen();

        assert_eq!(
            mask,
            vec![0],
            "expected exactly the tabline row masked once a second tab is open, got {mask:?}"
        );
        assert_eq!(
            view_screen.rows.len(),
            ref_screen.rows.len(),
            "expected both sides' canvases to agree on total row count once the tabline \
             reserves a row: view {view_screen:?}\nreference {ref_screen:?}"
        );

        let view_state = snapshot(&mut engine_side).expect("snapshot EngineSession");
        let ref_state = snapshot(&mut reference_side).expect("snapshot ReferenceSession");
        let divergences = compare(&view_state, &ref_state, &view_screen, &ref_screen, &mask);

        assert!(
            divergences.is_empty(),
            "engine/reference parity check found divergences after a second tab opened: \
             {divergences:?}\nview screen: {view_screen:?}\nreference screen: {ref_screen:?}\nmask: {mask:?}"
        );
    }

    /// Regression pin for the blocked-probe wedge (`corpus/fuzz-42-6.toml`,
    /// found by seeded fuzz): a script ending with a bare `t` leaves nvim
    /// blocked waiting for the motion's character argument, a state that
    /// defers every non-fast request -- so the old all-eval `snapshot`
    /// timed out on its first probe and the whole run failed as an ERROR.
    /// `snapshot` must instead capture the blocked state itself and still
    /// read the rest of the session's state through the `<Esc>` dismissal.
    #[test]
    fn snapshot_answers_in_a_blocked_char_wait_instead_of_wedging() {
        let mut session = testenv::spawning(|| EngineSession::spawn(40, 10))
            .expect("EngineSession::spawn against real nvim");
        while session.pump_until_flush(Duration::from_millis(500)) {}

        session
            .input("ihello<Esc>0t")
            .expect("input against EngineSession");
        assert!(session.pump_until_flush(QUIESCE_DEADLINE));
        while session.pump_until_flush(Duration::from_millis(500)) {}

        let state = snapshot(&mut session).expect("snapshot against a blocked session");

        assert!(
            state.blocked,
            "a pending t character argument must be captured as blocked: {state:?}"
        );
        assert_eq!(
            state.mode, "n",
            "nvim reports normal mode while blocked on a motion argument"
        );
        // the eval probes below the dismissal must still see the state the
        // script produced: the typed line, the cursor parked by `0`, and no
        // register the aborted `t` could have touched
        assert_eq!(state.buffer_lines, vec!["hello".to_string()]);
        assert_eq!(
            state.cursor,
            (1, 1),
            "the <Esc> dismissal must not move the cursor: {state:?}"
        );
        assert!(
            state.registers.iter().all(|(_, value)| value.is_empty()),
            "no probed register should hold anything after ihello<Esc>0t: {:?}",
            state.registers
        );
    }

    /// Spawns a real `EngineSession` against the pinned nvim, moves the
    /// cursor to a known position and sets a known buffer-local mark plus a
    /// known global mark, then asserts `snapshot()` reads back those exact
    /// values through `parse_cursor`/`parse_marks`. An nvim pin bump that
    /// changes the `getpos`/`getmarklist` reply shape (a field reordered, a
    /// type changed) fails this test loudly instead of silently degrading
    /// through a placeholder value a differential could compare away. The
    /// global mark also proves live that `snapshot`'s no-arg
    /// `getmarklist()` call actually merges global marks into the same
    /// result, not just buffer-local ones. Expected values captured live
    /// against the pinned nvim before this test was written: after
    /// `ihello<Esc>mamA`, the cursor and both the `a` and `A` marks land on
    /// line 1, column 5 (1-indexed byte column, past the typed `hello`).
    #[test]
    fn parse_cursor_and_parse_marks_match_a_live_getpos_and_getmarklist_reply() {
        let mut session = testenv::spawning(|| EngineSession::spawn(40, 10))
            .expect("EngineSession::spawn against real nvim");
        while session.pump_until_flush(Duration::from_millis(500)) {}

        session
            .input("ihello<Esc>mamA")
            .expect("input against EngineSession");
        assert!(session.pump_until_flush(QUIESCE_DEADLINE));
        while session.pump_until_flush(Duration::from_millis(500)) {}

        let state = snapshot(&mut session).expect("snapshot EngineSession");

        assert_eq!(
            state.cursor,
            (1, 5),
            "expected cursor at line 1 col 5 after ihello<Esc>mamA: {:?}",
            state.cursor
        );
        assert!(
            state
                .marks
                .iter()
                .any(|(name, row, col)| name == "'a" && *row == 1 && *col == 5),
            "expected buffer-local mark 'a at (1, 5): {:?}",
            state.marks
        );
        assert!(
            state
                .marks
                .iter()
                .any(|(name, row, col)| name == "'A" && *row == 1 && *col == 5),
            "expected global mark 'A at (1, 5), merged in by the no-arg \
             getmarklist() probe: {:?}",
            state.marks
        );
    }
}
