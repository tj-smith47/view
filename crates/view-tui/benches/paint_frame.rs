//! Hot-path micro-bench: the per-frame paint the runtime loop pays on every
//! redraw -- `render()`'s `Surface`, a composite into the persistent shadow,
//! then the shadow's diff of what changed against what the terminal shows --
//! against a populated 120x40 grid.
//!
//! **These numbers are relative instruments, not absolute per-keystroke
//! costs.** Every function below except the cold ones runs its frames back
//! to back, which keeps the whole paint path's working set resident; steady
//! typing leaves the core idle between frames and pays cold. The same frame
//! measures 2.94us hot and 21.27us cold, so a hot number understates what a
//! keystroke actually costs by roughly 7x. Use them to tell whether a change
//! made the code worse, never to answer how long a keystroke takes.
//!
//! Seven functions bracket the damage-clipping lever:
//!
//! - `paint_frame_full_recomposite` recomposites every cell and diffs the
//!   whole grid each frame (the pre-clipping behavior), the "before" number.
//! - `paint_frame_full_recomposite_wide` does the same over a grid of
//!   two-column glyphs. Every cost in the paint path that a symbol's width
//!   or byte length gates is free on the all-ASCII grid and charged here, so
//!   this is the function that sees a regression the ASCII one cannot.
//! - `paint_frame_steady_state` mutates one cell per iteration (the echo
//!   scenario's per-keystroke damage shape) and composites only the damaged
//!   row, the "after" number for a single typed character.
//! - `paint_frame_steady_state_crossterm` runs the clipped path over a real
//!   `CrosstermBackend` so its escape-encoding cost is included.
//! - `paint_frame_cold/steady_state_crossterm_cold` is that same frame with
//!   a keystroke interval of idle before each one, which is the state the
//!   editor actually meets.
//! - `paint_frame_agent_composer` (and its cold twin) is one character
//!   typed into the agent panel's composer while the panel is open and
//!   full height: the frame whose damage must be the composer's row rather
//!   than every row the panel covers.
//! - `paint_frame_prompt_overlay_open` is the modal prompt overlay's own
//!   first frame: the `MsgShow`+`CmdlineShow` pair `update()` turns into a
//!   pushed `OverlayKind::Prompt`, composited and diffed against an already
//!   painted grid, the exact transition a `:confirm()` or swapfile ATTENTION
//!   dialog costs the paint loop. Cold by construction (a fresh model per
//!   iteration, `KEYSTROKE_GAP`-paced): a prompt opening is a rare, one-shot
//!   event, never a steady per-frame path, so there is no hot variant.
//!
//! All seven absorb terminal write syscalls into their backend.

#![allow(clippy::expect_used)]

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use ratatui::backend::{Backend, TestBackend};
use ratatui::layout::Rect;
use std::hint::black_box;
use view_core::events::UiEvent;
use view_core::grid::GridOp;
use view_core::model::Model;
use view_core::msg::Msg;
use view_core::update::update;
use view_tui::paint::{Damage, Shadow};

/// Idle between frames in the cold variant. The `echo` bench row paces its
/// keystrokes at this interval and a human types slower still, so this is
/// the gap the paint path actually meets rather than a pathological one.
const KEYSTROKE_GAP: Duration = Duration::from_millis(10);

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// A model whose grid looks like a real editing session: full-width text
/// rows under a handful of highlight ids, so the compositor's per-cell style
/// resolution runs against a populated table rather than an all-default
/// fast path.
fn populated_model() -> Model {
    let mut model = Model::new();
    // a model left at the default 0x0 resolves every native overlay to a
    // 0x0 rect, which lays out no rows and paints no cells: an arm built on
    // one times a first paint rather than the overlay it names
    model.term_width = WIDTH;
    model.term_height = HEIGHT;
    model.engine.apply_grid(GridOp::Resize {
        width: WIDTH,
        height: HEIGHT,
    });
    for row in 0..HEIGHT {
        let cells = (0..WIDTH)
            .map(|col| {
                let ch = char::from(b'a' + ((row + col) % 26) as u8);
                (ch.to_string(), u64::from((row + col) % 7), 1)
            })
            .collect();
        model.engine.apply_grid(GridOp::PutLine {
            row,
            col_start: 0,
            cells,
        });
    }
    model.content_painted = true;
    model
}

/// The same grid filled with two-column CJK ideographs, each followed by the
/// blank cell nvim sends for the column the glyph covers.
///
/// The all-ASCII grid above leaves every width-dependent cost in the paint
/// path unmeasured: a single-byte symbol is one column wide by definition, so
/// the fit check short-circuits and the diff advances one cell at a time.
/// This grid charges for both, and is the shape a CJK, emoji, or box-drawing
/// buffer actually has on the wire.
fn wide_model() -> Model {
    let mut model = Model::new();
    model.engine.apply_grid(GridOp::Resize {
        width: WIDTH,
        height: HEIGHT,
    });
    for row in 0..HEIGHT {
        let cells = (0..WIDTH)
            .map(|col| {
                let text = if col % 2 == 0 {
                    // CJK unified ideographs, cycling so the row is not one
                    // repeated symbol
                    char::from_u32(0x4E00 + u32::from((row + col) % 128))
                        .unwrap_or('\u{4E00}')
                        .to_string()
                } else {
                    // the blank nvim sends for the column the glyph covers
                    " ".to_string()
                };
                (text, u64::from((row + col) % 7), 1)
            })
            .collect();
        model.engine.apply_grid(GridOp::PutLine {
            row,
            col_start: 0,
            cells,
        });
    }
    model.content_painted = true;
    model
}

/// One production-shaped frame: composite `damage` into the shadow, emit the
/// cells that changed against what the terminal shows, then promote the
/// composed frame -- exactly the sequence `Term::draw_surface` runs.
fn emit_frame<B: Backend>(
    backend: &mut B,
    shadow: &mut Shadow,
    model: &Model,
    surface: &view_surface::Surface,
    damage: &Damage,
) {
    shadow.compose(model, surface, damage);
    let _ = shadow.emit_updates(backend);
    shadow.commit();
}

/// A shadow sized for the bench grid with one full frame already painted, so
/// every measured iteration starts from a populated terminal rather than a
/// blank one.
fn primed_shadow<B: Backend>(backend: &mut B, model: &mut Model) -> Shadow {
    let mut shadow = Shadow::new();
    shadow.resize(Rect::new(0, 0, WIDTH, HEIGHT));
    let _ = model.take_paint_damage();
    let surface = view_surface::render(model);
    let _ = shadow.overlay_damage(&surface);
    emit_frame(backend, &mut shadow, model, &surface, &Damage::full());
    shadow
}

/// The "before" reference: recomposite and diff every cell each frame, the
/// whole-grid cost the clipping lever removes.
fn bench_paint_frame_full(c: &mut Criterion) {
    let mut model = populated_model();
    let mut backend = TestBackend::new(WIDTH, HEIGHT);
    let mut shadow = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    c.bench_function("paint_frame_full_recomposite", |b| {
        b.iter(|| {
            flip = !flip;
            model.engine.apply_grid(GridOp::PutLine {
                row: 5,
                col_start: 5,
                cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
            });
            let _ = model.take_paint_damage();
            let surface = view_surface::render(&model);
            emit_frame(
                black_box(&mut backend),
                black_box(&mut shadow),
                &model,
                &surface,
                &Damage::full(),
            );
        });
    });
}

/// The wide-glyph counterpart of the reference above: the same whole-frame
/// recomposite over a grid of two-column glyphs, so the paint path's
/// width-dependent costs stay measured.
fn bench_paint_frame_full_wide(c: &mut Criterion) {
    let mut model = wide_model();
    let mut backend = TestBackend::new(WIDTH, HEIGHT);
    let mut shadow = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    c.bench_function("paint_frame_full_recomposite_wide", |b| {
        b.iter(|| {
            flip = !flip;
            model.engine.apply_grid(GridOp::PutLine {
                row: 5,
                col_start: 4,
                cells: vec![
                    ((if flip { "界" } else { "語" }).to_string(), 0, 1),
                    (" ".to_string(), 0, 1),
                ],
            });
            let _ = model.take_paint_damage();
            let surface = view_surface::render(&model);
            emit_frame(
                black_box(&mut backend),
                black_box(&mut shadow),
                &model,
                &surface,
                &Damage::full(),
            );
        });
    });
}

/// The "after" number: one changed cell per frame, composited and diffed
/// through the same row-clipped `Damage` the runtime builds.
fn bench_paint_frame(c: &mut Criterion) {
    let mut model = populated_model();
    let mut backend = TestBackend::new(WIDTH, HEIGHT);
    let mut shadow = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    c.bench_function("paint_frame_steady_state", |b| {
        b.iter(|| {
            // one changed cell per frame, alternating so no iteration is a
            // no-op diff
            flip = !flip;
            model.engine.apply_grid(GridOp::PutLine {
                row: 5,
                col_start: 5,
                cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
            });
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let overlay_damage = shadow.overlay_damage(&surface);
            let damage =
                Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, false);
            emit_frame(
                black_box(&mut backend),
                black_box(&mut shadow),
                &model,
                &surface,
                &damage,
            );
        });
    });
}

/// Same clipped steady-state frame, but through `CrosstermBackend` over an
/// in-memory writer: includes the real escape-sequence generation the
/// production path pays, still minus terminal write syscalls, so the
/// difference against `paint_frame_steady_state` isolates crossterm's
/// per-frame encoding cost.
fn bench_paint_frame_crossterm(c: &mut Criterion) {
    let mut model = populated_model();
    let mut backend = ratatui::backend::CrosstermBackend::new(Vec::<u8>::new());
    let mut shadow = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    c.bench_function("paint_frame_steady_state_crossterm", |b| {
        b.iter(|| {
            flip = !flip;
            model.engine.apply_grid(GridOp::PutLine {
                row: 5,
                col_start: 5,
                cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
            });
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            let overlay_damage = shadow.overlay_damage(&surface);
            let damage =
                Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, false);
            emit_frame(
                black_box(&mut backend),
                black_box(&mut shadow),
                &model,
                &surface,
                &damage,
            );
        });
    });
}

/// The same frame as `paint_frame_steady_state_crossterm`, but with a
/// keystroke interval of idle before each one and only the frame itself
/// timed.
///
/// The tapped production run puts the two stages this covers at 28.4us p50
/// while the back-to-back version of it measures 2.9us. A tight loop keeps
/// every cache line, branch predictor entry and page mapping the paint path
/// touches resident, and steady typing never presents that state: a human
/// leaves the core idle between keystrokes and each frame starts cold. This
/// function is the falsifiable form of that explanation -- if the gap is
/// cache residency, the cost here rises toward the tapped number, and if it
/// does not, the explanation is wrong and the difference is elsewhere.
fn bench_paint_frame_cold(c: &mut Criterion) {
    let mut model = populated_model();
    let mut backend = ratatui::backend::CrosstermBackend::new(Vec::<u8>::new());
    let mut shadow = primed_shadow(&mut backend, &mut model);

    let mut flip = false;
    let mut group = c.benchmark_group("paint_frame_cold");
    // one frame per KEYSTROKE_GAP, so the sample count is what bounds the
    // wall clock rather than criterion's usual throughput target
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(12));
    group.bench_function("steady_state_crossterm_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                flip = !flip;
                model.engine.apply_grid(GridOp::PutLine {
                    row: 5,
                    col_start: 5,
                    cells: vec![((if flip { "x" } else { "y" }).to_string(), 0, 1)],
                });
                let started = Instant::now();
                let grid_damage = model.take_paint_damage();
                let surface = view_surface::render(&model);
                let overlay_damage = shadow.overlay_damage(&surface);
                let damage =
                    Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, false);
                emit_frame(
                    black_box(&mut backend),
                    black_box(&mut shadow),
                    &model,
                    &surface,
                    &damage,
                );
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

/// A model with the agent panel open, focused, and holding a transcript
/// long enough that a naive repaint would scale with it.
fn agent_panel_model() -> Model {
    let mut model = populated_model();
    model.ai_trusted = true;
    let _ = update(
        &mut model,
        Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: "open".to_string(),
        },
    );
    let panel = model.ai_panel_mut();
    for i in 0..200 {
        panel.transcript.append_or_extend(
            Some(&format!("m{i}")),
            "a transcript line with enough prose in it that wrapping and \
             styling both have real work to do",
            view_core::native::ai_panel::TranscriptRole::Agent,
        );
    }
    assert!(model.ai_panel().focused, "the composer holds focus");
    let surface = view_surface::render(&model);
    let rows = view_tui::paint::agent_panel_rows(&surface);
    assert_eq!(
        rows.len(),
        usize::from(HEIGHT),
        "the panel is on screen and full height: {rows:?}"
    );
    model
}

/// One character typed into the agent panel's composer.
///
/// The panel is full height, so this is the frame that proves a composer
/// keystroke costs the row it changed rather than the screen the panel
/// covers: the pre-clipping shape measured 539us against 88us at 263x88,
/// and it grew with the terminal instead of with the typing. Hot and cold
/// both, for the same reason `paint_frame_cold` exists -- typing is a cold
/// path, and the panel's own layout is the largest thing in it.
fn bench_paint_frame_agent_composer(c: &mut Criterion) {
    let mut model = agent_panel_model();
    let mut backend = ratatui::backend::CrosstermBackend::new(Vec::<u8>::new());
    let mut shadow = primed_shadow(&mut backend, &mut model);

    let mut typed = 0_u32;
    let composer_frame =
        |model: &mut Model, shadow: &mut Shadow, backend: &mut _, typed: &mut u32| {
            // the input is cleared before it reaches the panel's width, so
            // every measured frame is the one-row composer edit this row
            // exists to price rather than a wrap moving the transcript
            // boundary
            *typed += 1;
            if (*typed).is_multiple_of(20) {
                let _ = model.ai_panel_mut().take_input();
            }
            let _ = update(
                model,
                Msg::Key(view_core::msg::Key {
                    notation: "x".to_string(),
                }),
            );
            let started = Instant::now();
            let grid_damage = model.take_paint_damage();
            let surface = view_surface::render(model);
            let overlay_damage = shadow.overlay_damage(&surface);
            let damage =
                Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, false);
            emit_frame(
                black_box(backend),
                black_box(shadow),
                model,
                &surface,
                &damage,
            );
            started.elapsed()
        };

    c.bench_function("paint_frame_agent_composer", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                elapsed += composer_frame(&mut model, &mut shadow, &mut backend, &mut typed);
            }
            elapsed
        });
    });

    let mut group = c.benchmark_group("paint_frame_agent_composer_cold");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(12));
    group.bench_function("agent_composer_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                elapsed += composer_frame(&mut model, &mut shadow, &mut backend, &mut typed);
            }
            elapsed
        });
    });
    group.finish();
}

/// A `:confirm()`-shaped modal prompt, opened against an already painted
/// grid: the `MsgShow` question followed by the `CmdlineShow` choice line,
/// exactly the two-event sequence `update()`'s `Msg::Redraw` handler turns
/// into a pushed `OverlayKind::Prompt` (see
/// `docs/prompt-overlay-wire-capture.md`, section 1).
fn open_prompt_overlay(model: &mut Model) {
    let _ = update(
        model,
        Msg::Redraw(vec![
            UiEvent::MsgShow {
                kind: "confirm".to_string(),
                content: vec![(16, "Save changes?".to_string())],
                replace_last: false,
            },
            UiEvent::CmdlineShow {
                content: vec![(0, String::new())],
                pos: 0,
                firstc: String::new(),
                prompt: "[Y]es, (N)o: ".to_string(),
                indent: 0,
                level: 1,
            },
        ]),
    );
}

/// Cold: the modal prompt overlay's own first frame, from a fresh model per
/// iteration so allocation and cache-cold costs are charged rather than
/// hidden by a warm working set -- a prompt opening is a one-shot event no
/// steady-state loop ever repeats back to back.
fn bench_paint_frame_prompt_overlay_open(c: &mut Criterion) {
    let mut group = c.benchmark_group("paint_frame_prompt_overlay_open");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(12));
    group.bench_function("confirm_overlay_first_frame_cold", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                std::thread::sleep(KEYSTROKE_GAP);
                let mut model = populated_model();
                let mut backend = ratatui::backend::CrosstermBackend::new(Vec::<u8>::new());
                let mut shadow = primed_shadow(&mut backend, &mut model);
                open_prompt_overlay(&mut model);

                let started = Instant::now();
                let grid_damage = model.take_paint_damage();
                let surface = view_surface::render(&model);
                let overlay_damage = shadow.overlay_damage(&surface);
                let damage =
                    Damage::from_frame(&grid_damage, model.chrome_rows(), &overlay_damage, false);
                emit_frame(
                    black_box(&mut backend),
                    black_box(&mut shadow),
                    &model,
                    &surface,
                    &damage,
                );
                elapsed += started.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_paint_frame_full,
    bench_paint_frame_full_wide,
    bench_paint_frame,
    bench_paint_frame_crossterm,
    bench_paint_frame_cold,
    bench_paint_frame_agent_composer,
    bench_paint_frame_prompt_overlay_open
);
criterion_main!(benches);
