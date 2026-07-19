//! The parity relation the corpus runner drives: state probes plus a masked
//! grid diff, comparing view's own session against [`crate::ReferenceSession`]
//! (or any other [`Probe`] source) rather than against nvim's ground truth
//! directly. [`ReferenceSession`]'s own module docs already cover why a
//! second independent applier is the differential; this module is the
//! comparison layer that turns two sides' state into a list of concrete
//! disagreements.
//!
//! Two comparison axes, kept separate because they diverge for different
//! reasons and a caller needs to tell them apart:
//! - [`Divergence::State`]: the two sides' `nvim_eval` state probes
//!   ([`StateSnapshot`]) disagree -- buffer text, cursor, mode, registers,
//!   or marks. A bug in `view`'s own input handling or engine wiring, not a
//!   rendering bug.
//! - [`Divergence::Grid`]: the two sides' rendered screen text disagrees at
//!   a specific row not excluded by [`masked_rows`]. A rendering/apply bug
//!   in `view`'s `Model`/`Grid`/`Surface` pipeline (the exact class
//!   [`crate::ReferenceSession`]'s `RefGrid` exists to catch).
//!
//! [`masked_rows`] excludes rows [`crate::ReferenceSession`] cannot ever
//! agree on by construction: its `RefGrid` never receives
//! `Cmdline*`/`Msg*`/`Tabline*`/`Popupmenu*` content (see
//! `ReferenceSession::apply`'s own doc comment), so any row view's real
//! `Surface` paints one of those overlays into is not a real behavioral
//! disagreement, just an artifact of the reference applier's own scope.

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
}

impl Probe for EngineSession {
    fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        EngineSession::eval_str(self, expr)
    }
}

impl Probe for ReferenceSession {
    fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        ReferenceSession::eval_str(self, expr)
    }
}

/// One side's `nvim_eval` state, as read back through five fixed
/// probes: buffer text, cursor, mode, a fixed register set, and marks. Not a
/// full state dump (nvim has far more introspectable state than this): the
/// fields here are exactly what a scripted key-notation scenario can make
/// disagree between `view`'s own engine and [`ReferenceSession`]'s -- what
/// the buffer holds, where the cursor sits, what mode nvim is in, what a
/// yank/delete left in a register, and where a mark landed.
#[derive(Debug, Clone, PartialEq)]
pub struct StateSnapshot {
    pub buffer_lines: Vec<String>,
    pub cursor: (u64, u64),
    pub mode: String,
    pub registers: Vec<(char, String)>,
    pub marks: Vec<(String, u64, u64)>,
}

/// Probes `probe` for a full [`StateSnapshot`]: one `nvim_eval` round trip
/// for the buffer, one for the cursor, one for the mode, one per
/// [`REGISTER_NAMES`] entry, and one for marks. Every list-shaped probe
/// (buffer lines, marks) is joined vimscript-side with [`FIELD_SEP`]/
/// [`RECORD_SEP`] (or, for buffer lines, a literal newline -- no nvim buffer
/// line can ever contain an embedded `\n`, since that byte is exactly what
/// separates lines in nvim's own model) rather than shipped as a raw
/// `nvim_eval` array/dict result: `EngineHandle::eval_str` renders a
/// structured `Value` through `rmpv`'s own `Display`, a format this crate's
/// dependency-direction audit (`scripts/audit-deps.sh`) forbids parsing here
/// (no `serde`/`serde_json` allowed in `view-oracle`), so the join happens
/// on the nvim side instead, in a delimiter this code controls end to end.
///
/// # Errors
///
/// Returns [`OracleError`] if any of the underlying `eval_str` calls fail.
pub fn snapshot(probe: &mut dyn Probe) -> Result<StateSnapshot, OracleError> {
    let buffer_lines = probe
        .eval_str("join(getline(1,'$'), \"\\n\")")?
        .split('\n')
        .map(str::to_string)
        .collect();

    let cursor_raw = probe.eval_str(&format!("join(getpos('.'), \"{FIELD_SEP}\")"))?;
    let cursor = parse_cursor(&cursor_raw);

    let mode = probe.eval_str("mode(1)")?;

    let mut registers = Vec::with_capacity(REGISTER_NAMES.len());
    for name in REGISTER_NAMES {
        let value = probe.eval_str(&format!("getreg('{name}')"))?;
        registers.push((name, value));
    }

    let marks_raw = probe.eval_str(&format!(
        "join(map(getmarklist(bufnr('%')), 'v:val.mark . \"{FIELD_SEP}\" . v:val.pos[1] . \"{FIELD_SEP}\" . v:val.pos[2]'), \"{RECORD_SEP}\")"
    ))?;
    let marks = parse_marks(&marks_raw);

    Ok(StateSnapshot {
        buffer_lines,
        cursor,
        mode,
        registers,
        marks,
    })
}

/// Parses a `join(getpos('.'), FIELD_SEP)` reply (`bufnum, lnum, col, off`)
/// into `(lnum, col)`: [`StateSnapshot::cursor`] only ever needs the
/// position, not which buffer or the virtualedit offset. An unparseable or
/// short reply degrades to `(0, 0)` rather than panicking: a live-engine
/// probe returning a malformed reply is a real divergence for [`compare`]
/// to report, not a reason to abort the whole snapshot.
fn parse_cursor(raw: &str) -> (u64, u64) {
    let mut fields = raw.split(FIELD_SEP);
    let _bufnum = fields.next();
    let line = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let col = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (line, col)
}

/// Parses a `join(map(getmarklist(...), ...), RECORD_SEP)` reply into
/// `(mark name, row, col)` triples. An empty reply (no marks at all, which
/// cannot happen once nvim has set its own automatic marks but is the
/// correct degenerate case) yields an empty `Vec` rather than a one-element
/// `Vec` holding an empty record: `"".split(RECORD_SEP)` would otherwise
/// yield one empty string, which `parse_marks`'s inner field split would
/// then fail to parse into a row/col and silently `filter_map` away anyway
/// -- checking `is_empty()` up front says so directly instead of relying on
/// that fallthrough.
fn parse_marks(raw: &str) -> Vec<(String, u64, u64)> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(RECORD_SEP)
        .filter_map(|record| {
            let mut fields = record.split(FIELD_SEP);
            let name = fields.next()?.to_string();
            let row = fields.next().and_then(|s| s.parse().ok())?;
            let col = fields.next().and_then(|s| s.parse().ok())?;
            Some((name, row, col))
        })
        .collect()
}

/// One concrete disagreement between `view`'s side and the reference side,
/// found by [`compare`]. Two arms because a state-probe disagreement and a
/// rendered-row disagreement point at different layers of the stack (see
/// this module's own doc comment) and a caller triaging a failure needs to
/// know immediately which kind it is looking at, not just that the two
/// sides disagreed somewhere.
#[derive(Debug)]
pub enum Divergence {
    /// A [`StateSnapshot`] field disagreed; `field` names which one
    /// (`"buffer_lines"`, `"cursor"`, `"mode"`, `"registers"`, or
    /// `"marks"`).
    State {
        field: String,
        view: String,
        reference: String,
    },
    /// Rendered row `row` disagreed and was not in the caller's mask.
    Grid {
        row: u16,
        view: String,
        reference: String,
    },
}

/// Diffs `view_state` against `ref_state` field by field, then
/// `view_rows`/`ref_rows` row by row, skipping any row index present in
/// `mask` (see [`masked_rows`]) -- the ordering (state first, then grid) is
/// arbitrary and not load-bearing; every field/row that disagrees produces
/// its own [`Divergence`], so this always returns the *complete* set of
/// disagreements between the two sides, not just the first.
///
/// A row present in only one of `view_rows`/`ref_rows` (the two sides
/// disagreeing on total row count) is treated as disagreeing against an
/// empty string on the shorter side, rather than being silently skipped:
/// a row count mismatch is itself real signal a differential oracle must
/// surface.
#[must_use]
pub fn compare(
    view_state: &StateSnapshot,
    ref_state: &StateSnapshot,
    view_rows: &[String],
    ref_rows: &[String],
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

    let row_count = view_rows.len().max(ref_rows.len());
    for index in 0..row_count {
        let Ok(row) = u16::try_from(index) else {
            continue;
        };
        if mask.contains(&row) {
            continue;
        }
        let view = view_rows.get(index).map_or("", String::as_str);
        let reference = ref_rows.get(index).map_or("", String::as_str);
        if view != reference {
            divergences.push(Divergence::Grid {
                row,
                view: view.to_string(),
                reference: reference.to_string(),
            });
        }
    }

    divergences
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

    fn snapshot_fixture() -> StateSnapshot {
        StateSnapshot {
            buffer_lines: vec!["hello".to_string(), "world".to_string()],
            cursor: (1, 0),
            mode: "n".to_string(),
            registers: vec![('"', "hello\n".to_string()), ('0', "hello\n".to_string())],
            marks: vec![("'.".to_string(), 1, 0)],
        }
    }

    fn rows_fixture() -> Vec<String> {
        vec!["hello     ".to_string(), "world     ".to_string()]
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

        let divergences = compare(&view_state, &ref_state, &rows, &rows, &[]);

        assert_eq!(divergences.len(), 1, "divergences: {divergences:?}");
        match &divergences[0] {
            Divergence::State { field, .. } => assert_eq!(field, "registers"),
            other => unreachable!("expected Divergence::State, got {other:?}"),
        }
    }

    /// Arm 2: a corrupted, unmasked grid row (state identical) must produce
    /// exactly one `Divergence::Grid` naming that row.
    #[test]
    fn corrupted_grid_row_produces_exactly_one_grid_divergence() {
        let state = snapshot_fixture();
        let view_rows = rows_fixture();
        let mut ref_rows = rows_fixture();
        ref_rows[1] = "CORRUPTED ".to_string();

        let divergences = compare(&state, &state, &view_rows, &ref_rows, &[]);

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

        let divergences = compare(&state, &state, &view_rows, &ref_rows, &[1]);

        assert!(
            divergences.is_empty(),
            "masked row still produced a divergence: {divergences:?}"
        );
    }

    /// Arm 4: identical state and rows on both sides, no mask, must produce
    /// nothing -- the non-tautology baseline the other three arms are
    /// deviations from.
    #[test]
    fn identical_inputs_produce_no_divergence() {
        let state = snapshot_fixture();
        let rows = rows_fixture();

        let divergences = compare(&state, &state, &rows, &rows, &[]);

        assert!(
            divergences.is_empty(),
            "identical inputs produced a divergence: {divergences:?}"
        );
    }

    /// A row present on one side only (a row-count mismatch) must still
    /// surface as a `Grid` divergence rather than being silently skipped.
    #[test]
    fn row_count_mismatch_diverges_against_an_empty_string() {
        let state = snapshot_fixture();
        let view_rows = rows_fixture();
        let ref_rows = vec![view_rows[0].clone()];

        let divergences = compare(&state, &state, &view_rows, &ref_rows, &[]);

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
        assert_eq!(parse_cursor(&raw), (3, 5));
    }

    #[test]
    fn parse_cursor_degrades_to_zero_zero_on_a_malformed_reply() {
        assert_eq!(parse_cursor(""), (0, 0));
        assert_eq!(parse_cursor("not-a-number"), (0, 0));
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

        let marks = parse_marks(&raw);

        assert_eq!(
            marks,
            vec![("'a".to_string(), 2, 3), ("'.".to_string(), 1, 1),]
        );
    }

    #[test]
    fn parse_marks_reads_an_empty_reply_as_no_marks() {
        assert!(parse_marks("").is_empty());
    }

    fn model_with_grid(width: u16, height: u16) -> Model {
        let mut model = Model::new();
        model.engine.grid.apply(GridOp::Resize { width, height });
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
        let mut engine_side =
            EngineSession::spawn(40, 10).expect("EngineSession::spawn against real nvim");
        let mut reference_side =
            ReferenceSession::spawn(40, 10).expect("ReferenceSession::spawn against real nvim");

        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

        engine_side
            .input("ihello<Esc>yyp")
            .expect("input against EngineSession");
        reference_side
            .input("ihello<Esc>yyp")
            .expect("input against ReferenceSession");

        assert!(engine_side.pump_until_flush(QUIESCE_DEADLINE));
        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

        let view_surface = engine_side.surface();
        let view_rows = engine_side.screen_rows();
        let mask = masked_rows(&view_surface);
        let ref_rows = reference_side.screen_rows();

        let view_state = snapshot(&mut engine_side).expect("snapshot EngineSession");
        let ref_state = snapshot(&mut reference_side).expect("snapshot ReferenceSession");

        let divergences = compare(&view_state, &ref_state, &view_rows, &ref_rows, &mask);

        assert!(
            divergences.is_empty(),
            "engine/reference parity check found divergences: {divergences:?}\n\
             view state: {view_state:?}\nref state: {ref_state:?}\n\
             view rows: {view_rows:?}\nref rows: {ref_rows:?}\nmask: {mask:?}"
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
    }
}
