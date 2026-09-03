//! Captures the redraw vocabulary the pinned engine emits under
//! `ext_multigrid`, and pins `docs/multigrid-wire-capture.md` to it.
//!
//! The capture is a test rather than a one-shot script so an engine-pin
//! bump re-runs it: an event this engine grows, or one it stops emitting,
//! parts the doc from the live stream and fails here instead of leaving a
//! document that describes an engine nobody runs any more.
//!
//! The multigrid and single-grid arms drive the identical script against
//! the identical isolated engine and differ only in the attach option set,
//! so the delta between the two transcripts is a fact about
//! `ext_multigrid` and not about the script. The single-grid arm is also
//! the collector's positive control: several of the doc's claims are
//! absences under multigrid (grid 1 no longer carrying window text, most
//! of all), and an absence is only readable next to an arm where the same
//! collector saw the event. A third, short-lived arm ([`probe_extras`])
//! covers the two questions neither of those can reach.
//!
//! Unlike every other live driver in this crate, the connection here is a
//! bare [`EngineHandle::start`] over a child this file spawns, not
//! `Engine::spawn`: a pumped connection routes every `redraw` notification
//! through the damage pump, which decodes it, and a decoded stream cannot
//! answer what an *undecoded* event's argument tuple looks like -- which is
//! the whole subject. That constructor's unbounded notification channel is
//! the only place in this workspace where the verbatim wire values are
//! still readable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use view_engine::process::EngineConfig;
use view_engine::ui_events::{decode_redraw, UiEvent};
use view_engine::{EngineHandle, EngineNotification};

/// The terminal size both arms attach at. Wide enough that a `:vsplit`
/// leaves two windows whose column offsets differ, so a `win_pos` column
/// is distinguishable from a row in the capture.
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// The size the `nvim_ui_try_resize` scenario asks for; different from
/// [`COLS`]/[`ROWS`] in both axes so neither a width nor a height that
/// failed to reach a window grid can hide behind the other.
const RESIZE_COLS: u16 = 70;
const RESIZE_ROWS: u16 = 20;

/// Where the mouse scenario clicks: a column inside the right-hand window
/// of a fresh `:vsplit` at [`COLS`] wide, and a row inside its text area.
const CLICK_ROW: u16 = 3;
const CLICK_COL: u16 = 60;

/// How long a step's redraw traffic may keep arriving before the collector
/// calls it settled, and how long it waits for the first `flush` that says
/// the step produced traffic at all. Both host-scaled: the engine's redraw
/// cadence stretches with the machine.
const QUIET: Duration = Duration::from_millis(80);
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// The doc this capture writes and then holds itself to.
const DOC: &str = "docs/multigrid-wire-capture.md";

/// One redraw event, as one wire tuple: the event name and its argument
/// tuple rendered verbatim.
struct WireEvent {
    name: String,
    args: String,
}

/// One scripted step and what the engine did in response to it.
struct Step {
    label: &'static str,
    events: Vec<WireEvent>,
    /// `nvim_list_wins` with the position, size and `relative` field nvim
    /// itself answers for each window, read at this step's settle point.
    /// Independent of the redraw stream, which is the point: a pane
    /// registry derived from `win_pos` and `win_float_pos` is checked
    /// against this, never against itself.
    windows: String,
}

/// Everything one arm's run produced.
struct Transcript {
    steps: Vec<Step>,
    /// Event names that reached [`decode_redraw`] and came back as
    /// [`UiEvent::Unknown`]: exactly the vocabulary view has no decoded
    /// variant for today.
    undecoded: Vec<String>,
    /// `nvim --api-info`'s `ui_events` entries for the grid and window
    /// events, as the engine itself serializes them. The doc's declared
    /// argument names and types come from here rather than from the
    /// observed values.
    ui_events_meta: String,
    /// The same metadata for `nvim_input_mouse` and the two
    /// `nvim_ui_try_resize` calls the doc has to answer for.
    ui_call_meta: String,
    /// What the mouse steps proved: for each of the two addressings, the
    /// window before the click, the click, and the window after.
    mouse_outcome: String,
    /// The grid and grid-local column the second click was addressed to,
    /// and the window nvim made current in answer to it.
    grid_addressed: (u64, u16, String),
    /// The window the first, `grid=0` click made current.
    screen_addressed: String,
}

impl Transcript {
    /// Every event name this arm emitted, sorted and deduplicated: the form
    /// the doc publishes and this test pins.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .steps
            .iter()
            .flat_map(|step| step.events.iter().map(|event| event.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The whole run as one line per wire tuple, prefixed by the step that
    /// produced it: the artifact the doc is written from, and the value the
    /// two arms are compared as.
    fn dump(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            let _ = writeln!(out, "# {} -- windows: {}", step.label, step.windows);
            for event in &step.events {
                let _ = writeln!(out, "{} {}", event.name, event.args);
            }
        }
        out
    }
}

/// A child engine plus the raw notification channel its redraw traffic
/// arrives on. Reaps its own child on drop: this harness shares a process
/// table with peer sessions, so the only pid it may signal is its own.
struct RawUi {
    child: Child,
    handle: EngineHandle,
    notifications: Receiver<EngineNotification>,
    undecoded: Vec<String>,
}

impl Drop for RawUi {
    fn drop(&mut self) {
        // no graceful `qa!` first: this child holds nothing worth saving,
        // and a wait on an engine that never answers would hang the suite
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl RawUi {
    /// Spawns an isolated `nvim --embed` and speaks msgpack-RPC to it over
    /// its own pipes.
    ///
    /// The argument list is `--embed` plus [`EngineConfig::isolated`]'s own
    /// `--clean -n`, and the environment is that config's inspectable
    /// `env_plan` applied entry by entry, so the child's isolation is the
    /// one every other live driver here gets. It omits the one thing
    /// `Engine::spawn` adds on top, the swap-recovery `--cmd` guard, which
    /// has nothing to answer for in a run that opens no file: every buffer
    /// this script creates is unnamed or scratch.
    fn spawn(surfaces: &[&str]) -> Self {
        let cfg = EngineConfig::isolated();
        view_engine::env::prepare_empty_search_path().unwrap();
        view_engine::env::prepare_hermetic_home().unwrap();
        let mut command = Command::new(&cfg.nvim_bin);
        command.arg("--embed").args(&cfg.extra_args);
        for (name, value) in cfg.env_plan() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("the pinned nvim must be on PATH");
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (handle, notifications) = EngineHandle::start(stdout, stdin);
        handle.ui_attach(COLS, ROWS, surfaces).unwrap();
        Self {
            child,
            handle,
            notifications,
            undecoded: Vec::new(),
        }
    }

    /// Drains every redraw notification the engine has queued for the step
    /// just issued.
    ///
    /// Every step is issued through a blocking call, so nvim has already
    /// run it by the time this is entered and its traffic is either queued
    /// or being written. The first `flush` says a redraw cycle closed; the
    /// quiet window after it catches a second cycle the same step
    /// scheduled. Both bounds are host-scaled, and each is a ceiling on a
    /// wait rather than a discriminator between two events, so a stall
    /// moves both of its ends together.
    fn collect(&mut self) -> Vec<WireEvent> {
        let deadline = Instant::now() + view_test_support::host_deadline(FLUSH_WAIT);
        let quiet = view_test_support::host_deadline(QUIET);
        let mut events = Vec::new();
        let mut flushed = false;
        loop {
            let wait = if flushed {
                quiet
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            if wait.is_zero() {
                return events;
            }
            match self.notifications.recv_timeout(wait) {
                Ok(notification) => {
                    if notification.method != "redraw" {
                        continue;
                    }
                    for decoded in decode_redraw(&notification.params) {
                        if let UiEvent::Unknown { name } = decoded {
                            if !self.undecoded.contains(&name) {
                                self.undecoded.push(name);
                            }
                        }
                    }
                    for event in render_batches(&notification) {
                        flushed |= event.name == "flush";
                        events.push(event);
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return events,
            }
        }
    }

    /// The window-layout ground truth at this moment.
    ///
    /// Formatted field by field rather than `json_encode`d: a vimscript
    /// dictionary serializes in its own hash order, and a committed
    /// artifact must not rest on that staying the same across builds.
    fn windows(&self) -> String {
        self.handle
            .eval_str(
                "join(map(nvim_list_wins(), {_, w -> printf(\
                 'win=%d tab=%d pos=[%d,%d] size=%dx%d relative=%s', \
                 w, nvim_win_get_tabpage(w), \
                 nvim_win_get_position(w)[0], nvim_win_get_position(w)[1], \
                 nvim_win_get_width(w), nvim_win_get_height(w), \
                 get(nvim_win_get_config(w), 'relative', ''))}), ' | ')",
            )
            .unwrap()
    }
}

/// Renders one `redraw` notification's batches into one entry per wire
/// tuple. The wire shape is `["name", [args...], [args...]]`: one name can
/// carry many tuples, and each tuple is one event.
///
/// The value rendering is a worklist inside this function rather than a
/// recursive helper because the wire value type belongs to `rmpv`, which
/// `scripts/audit-deps.sh` confines to `view-engine`: a helper would have
/// to name that type in its own signature, while locals infer it.
fn render_batches(notification: &EngineNotification) -> Vec<WireEvent> {
    let mut out = Vec::new();
    for batch in &notification.params {
        let Some(items) = batch.as_array() else {
            continue;
        };
        let Some((name, tuples)) = items.split_first() else {
            continue;
        };
        let Some(name) = name.as_str() else {
            continue;
        };
        for tuple in tuples {
            let mut args = String::new();
            let mut stack = vec![(tuple, 0_usize)];
            while let Some(&(value, index)) = stack.last() {
                if let Some(children) = value.as_array() {
                    if index == 0 {
                        args.push('[');
                    } else if index < children.len() {
                        args.push_str(", ");
                    }
                    if index < children.len() {
                        if let Some(frame) = stack.last_mut() {
                            frame.1 += 1;
                        }
                        stack.push((&children[index], 0));
                        continue;
                    }
                    args.push(']');
                } else if let Some((kind, payload)) = value.as_ext() {
                    let _ = write!(args, "ext({kind}:{})", ext_handle(payload));
                } else if let Some(text) = value.as_str() {
                    let _ = write!(args, "{text:?}");
                } else if value.is_f32() || value.is_f64() {
                    // a whole-numbered float prints as an integer through
                    // Display, and this doc's subject is which element of a
                    // tuple is which type
                    let _ = write!(args, "{:?}", value.as_f64().unwrap_or_default());
                } else {
                    args.push_str(&value.to_string());
                }
                stack.pop();
            }
            out.push(WireEvent {
                name: name.to_string(),
                args,
            });
        }
    }
    out
}

/// The integer a buffer/window/tabpage `Ext` payload carries: a msgpack
/// integer in its own right, nested inside the extension body.
fn ext_handle(payload: &[u8]) -> String {
    match payload {
        [single] if *single <= 0x7f => single.to_string(),
        [0xcc, byte] => byte.to_string(),
        [0xcd, high, low] => u16::from_be_bytes([*high, *low]).to_string(),
        [0xce, a, b, c, d] => u32::from_be_bytes([*a, *b, *c, *d]).to_string(),
        other => format!("{other:02x?}"),
    }
}

/// The grid the right-hand window of the standing `:vsplit` sits on, and
/// the screen column its first text cell occupies, read from the last
/// placement the run collected.
///
/// The single-grid arm announces no placement at all and so answers the
/// global grid at column 0 -- which is the addressing that arm exists to
/// be asked about, since a frontend with one grid still has to name it.
fn right_window(steps: &[Step]) -> (u64, u16) {
    let mut right = (1, 0);
    for step in steps {
        let placed: Vec<(u64, u16)> = step
            .events
            .iter()
            .filter(|event| event.name == "win_pos")
            .filter_map(|event| {
                let fields: Vec<&str> = event.args.trim_matches(['[', ']']).split(", ").collect();
                let grid = fields.first()?.parse().ok()?;
                let startcol = fields.get(3)?.parse().ok()?;
                Some((grid, startcol))
            })
            .collect();
        if let Some(rightmost) = placed.into_iter().max_by_key(|(_, col)| *col) {
            right = rightmost;
        }
    }
    right
}

/// Drives the whole script against one attach option set and returns
/// everything it produced.
fn run(surfaces: &[&str]) -> Transcript {
    let mut ui = RawUi::spawn(surfaces);
    let mut steps = Vec::new();
    let step = |ui: &mut RawUi, label: &'static str| {
        let events = ui.collect();
        let windows = ui.windows();
        Step {
            label,
            events,
            windows,
        }
    };

    steps.push(step(&mut ui, "attach"));

    for (label, command) in [
        ("vsplit", "vsplit"),
        ("split", "split"),
        ("wincmd w", "wincmd w"),
        ("resize 5", "resize 5"),
        ("close", "close"),
        ("only", "only"),
        ("vsplit again", "vsplit"),
    ] {
        ui.handle.command(command).unwrap();
        steps.push(step(&mut ui, label));
    }

    ui.handle.command("set mouse=a").unwrap();
    steps.push(step(&mut ui, "set mouse=a"));
    // the click lands in the right-hand window of the split above, through
    // view's own `input_mouse` and so with its constant grid 0: what that
    // constant means under multigrid is one of the questions
    let before = ui.handle.eval_str("nvim_get_current_win()").unwrap();
    ui.handle
        .input_mouse("left", "press", "", 0, CLICK_ROW, CLICK_COL)
        .unwrap();
    // a notification, so the blocking eval below is what proves nvim has
    // consumed it before the window is read back
    let after = ui.handle.eval_str("nvim_get_current_win()").unwrap();
    steps.push(step(&mut ui, "mouse press"));

    // the same screen cell addressed the other way: the grid the right-hand
    // window sits on, and the column inside that grid rather than on the
    // screen. Focus goes back to the left window first, so the outcome is
    // the same observable the grid=0 arm above produced.
    let (right_grid, right_col) = right_window(&steps);
    let grid_col = CLICK_COL - right_col;
    ui.handle.command("wincmd h").unwrap();
    steps.push(step(&mut ui, "wincmd h"));
    let before_grid = ui.handle.eval_str("nvim_get_current_win()").unwrap();
    ui.handle
        .input_mouse("left", "press", "", right_grid, CLICK_ROW, grid_col)
        .unwrap();
    let after_grid = ui.handle.eval_str("nvim_get_current_win()").unwrap();
    let mouse_outcome = format!(
        "current window before: {before}\n\
         nvim_input_mouse(\"left\", \"press\", \"\", grid=0, row={CLICK_ROW}, col={CLICK_COL})\n\
         current window after: {after}\n\
         right-hand window: grid={right_grid} at screen column {right_col}\n\
         current window before: {before_grid}\n\
         nvim_input_mouse(\"left\", \"press\", \"\", grid={right_grid}, \
         row={CLICK_ROW}, col={grid_col})\n\
         current window after: {after_grid}"
    );
    steps.push(step(&mut ui, "grid-addressed mouse press"));

    ui.handle.try_resize(RESIZE_COLS, RESIZE_ROWS).unwrap();
    steps.push(step(&mut ui, "nvim_ui_try_resize"));

    ui.handle
        .command(
            "lua vim.g.view_capture_float = vim.api.nvim_open_win(\
             vim.api.nvim_create_buf(false, true), false, \
             {relative='editor', row=2, col=4, width=20, height=3, border='single'})",
        )
        .unwrap();
    steps.push(step(&mut ui, "nvim_open_win float"));

    ui.handle
        .command("lua vim.api.nvim_win_close(vim.g.view_capture_float, true)")
        .unwrap();
    steps.push(step(&mut ui, "float close"));

    for (label, command) in [
        ("tabnew", "tabnew"),
        ("tabclose", "tabclose"),
        // `noswapfile` alongside the fill: `-n` sets `'updatecount'` only
        // once a UI has attached, and this buffer was created before that,
        // so it is still a buffer that would write a swap file into a
        // hermetic home concurrent runs share
        (
            "fill buffer",
            "set noswapfile | call setline(1, map(range(1, 200), 'string(v:val)'))",
        ),
        ("scroll", "execute \"normal! \\<C-e>\""),
    ] {
        ui.handle.command(command).unwrap();
        steps.push(step(&mut ui, label));
    }

    let ui_events_meta = ui
        .handle
        .eval_str(
            "json_encode(filter(copy(api_info().ui_events), \
             {_, e -> e.name =~# '^\\(grid_\\|win_\\|msg_set_pos\\)'}))",
        )
        .unwrap();
    let ui_call_meta = ui
        .handle
        .eval_str(
            "json_encode(map(filter(copy(api_info().functions), \
             {_, f -> f.name =~# '^nvim_\\(input_mouse\\|ui_try_resize\\)'}), \
             {_, f -> [f.name, f.parameters]}))",
        )
        .unwrap();

    let undecoded = ui.undecoded.clone();
    Transcript {
        steps,
        undecoded,
        ui_events_meta,
        ui_call_meta,
        mouse_outcome,
        grid_addressed: (right_grid, grid_col, after_grid),
        screen_addressed: after,
    }
}

/// The two questions the shared script cannot answer from inside itself,
/// because each needs an attach set or a call the script does not make.
///
/// `msg_set_pos` positions a message *grid*, and this workspace's attach
/// set externalizes messages, so its absence from the arms above is only
/// evidence next to an arm attached the other way. External windows are
/// the same shape: `win_external_pos` announces a window a UI has taken
/// out of the grid layout, which nvim only permits a UI that says it can
/// host one.
struct Probes {
    /// Every event name a multigrid UI *without* `ext_messages` emitted
    /// while the message area was made to move.
    without_ext_messages: Vec<String>,
    /// That arm's full transcript.
    dump: String,
    /// The names that arm left undecoded. It is the only arm that emits
    /// `win_external_pos` at all, so it is the only place a decoder for it
    /// can be proven against the engine rather than against a fixture.
    undecoded: Vec<String>,
    /// What nvim answered when a window was asked to go external.
    external: String,
}

fn probe_extras() -> Probes {
    let mut ui = RawUi::spawn(&["ext_linegrid", "ext_multigrid"]);
    let mut steps = vec![Step {
        label: "attach without ext_messages",
        events: ui.collect(),
        windows: ui.windows(),
    }];
    for (label, command) in [
        // both move the message area a multigrid UI would be told to
        // reposition, without leaving nvim on a hit-enter prompt no key
        // can answer here
        ("set cmdheight=3", "set cmdheight=3"),
        ("echo", "echo 'capture'"),
        ("set cmdheight=1", "set cmdheight=1"),
    ] {
        ui.handle.command(command).unwrap();
        steps.push(Step {
            label,
            events: ui.collect(),
            windows: ui.windows(),
        });
    }
    // a second window first: nvim refuses to take the last window out of
    // the layout for a reason that has nothing to do with the UI's answer
    ui.handle.command("vsplit").unwrap();
    let external = match ui
        .handle
        .command("lua vim.api.nvim_win_set_config(0, {external=true, width=20, height=5})")
    {
        Ok(()) => "accepted".to_string(),
        Err(err) => format!("refused: {err}"),
    };
    steps.push(Step {
        label: "nvim_win_set_config external=true",
        events: ui.collect(),
        windows: ui.windows(),
    });
    let transcript = Transcript {
        steps,
        undecoded: ui.undecoded.clone(),
        ui_events_meta: String::new(),
        ui_call_meta: String::new(),
        mouse_outcome: String::new(),
        grid_addressed: (0, 0, String::new()),
        screen_addressed: String::new(),
    };
    Probes {
        without_ext_messages: transcript.names(),
        dump: transcript.dump(),
        undecoded: ui.undecoded.clone(),
        external,
    }
}

/// Where a run leaves its full transcripts, for a later authoring pass to
/// read: the doc quotes from these, and nothing regenerates itself.
fn artifact_dir() -> PathBuf {
    let dir = view_oracle::target_root().join("multigrid-capture");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The fenced block that follows `heading` in the doc, as lines.
fn doc_fence(doc: &str, heading: &str) -> Vec<String> {
    let after = doc.split_once(heading).map(|(_, rest)| rest);
    assert!(after.is_some(), "{DOC} must carry the heading {heading:?}");
    let fenced = after
        .and_then(|rest| rest.split_once("```\n"))
        .map(|(_, body)| body);
    assert!(
        fenced.is_some(),
        "{DOC}'s {heading:?} section must open a fence"
    );
    let body = fenced
        .and_then(|body| body.split_once("\n```"))
        .map(|(body, _)| body);
    assert!(
        body.is_some(),
        "{DOC}'s {heading:?} fence must close on a line of its own"
    );
    body.unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn the_multigrid_vocabulary_the_doc_publishes_is_the_one_the_pinned_engine_emits() {
    let mut multigrid_surfaces = view_engine::UI_EXT_OPTIONS.to_vec();
    multigrid_surfaces.push("ext_multigrid");

    let multigrid = run(&multigrid_surfaces);
    let single = run(view_engine::UI_EXT_OPTIONS);
    let probes = probe_extras();

    let dir = artifact_dir();
    std::fs::write(dir.join("multigrid.txt"), multigrid.dump()).unwrap();
    std::fs::write(dir.join("single-grid.txt"), single.dump()).unwrap();
    std::fs::write(dir.join("no-ext-messages.txt"), &probes.dump).unwrap();
    std::fs::write(
        dir.join("metadata.txt"),
        format!(
            "ui_events:\n{}\n\ncalls:\n{}\n\nmouse, multigrid:\n{}\n\n\
             mouse, single-grid:\n{}\n\nexternal window:\n{}\n\nundecoded:\n{}\n",
            multigrid.ui_events_meta,
            multigrid.ui_call_meta,
            multigrid.mouse_outcome,
            single.mouse_outcome,
            probes.external,
            multigrid.undecoded.join("\n"),
        ),
    )
    .unwrap();

    let multigrid_names = multigrid.names();
    let single_names = single.names();

    // the disconfirming half: an attach silently ignores an option it does
    // not know, so two identical streams mean `ext_multigrid` was never
    // negotiated and every claim below would be about single-grid
    let only_multigrid: Vec<&String> = multigrid_names
        .iter()
        .filter(|name| !single_names.contains(name))
        .collect();
    assert!(
        !only_multigrid.is_empty(),
        "the multigrid arm emitted no event name the single-grid arm did not; \
         ext_multigrid was not negotiated. multigrid: {multigrid_names:?}"
    );
    assert_ne!(
        multigrid.dump(),
        single.dump(),
        "both arms produced an identical transcript, so the attach option \
         set made no difference to the wire"
    );
    assert!(
        !multigrid.undecoded.is_empty(),
        "every event the multigrid arm emitted already decodes to a typed \
         variant, so this capture describes the vocabulary view already \
         believes in rather than the one multigrid adds"
    );
    // the grid-addressed click proves nothing unless it was addressed
    // somewhere the screen-addressed one could not reach: a window grid of
    // nvim's own, at a column that is not the screen column
    let (grid, col, landed) = &multigrid.grid_addressed;
    assert!(
        *grid > 1 && *col != CLICK_COL,
        "the multigrid arm addressed grid {grid} column {col}, which is the \
         global grid or the screen column; the second click then tested the \
         same thing the first one did"
    );
    assert_eq!(
        landed, &multigrid.screen_addressed,
        "a click addressed to grid {grid} at its own column {col} reached a \
         different window than the same screen cell addressed globally, so \
         grid-local coordinates are not what nvim reads them as"
    );
    let (single_grid, single_col, single_landed) = &single.grid_addressed;
    assert_eq!(
        (*single_grid, *single_col),
        (1, CLICK_COL),
        "the single-grid arm must address the global grid at the screen \
         column: it has no window grid to name"
    );
    assert_eq!(
        single_landed, &single.screen_addressed,
        "naming the global grid explicitly reached a different window than \
         the grid=0 sentinel did, so a single-grid session cannot send its \
         own grid id"
    );
    // the only arm that emits `win_external_pos`: its decode is otherwise
    // proven against a hand-built tuple, which cannot catch a field order
    // this engine spells differently
    assert!(
        probes
            .without_ext_messages
            .contains(&"win_external_pos".to_string()),
        "the external-window probe emitted no win_external_pos, so nothing \
         here exercises that decoder: {:?}",
        probes.without_ext_messages
    );
    assert!(
        !probes.undecoded.contains(&"win_external_pos".to_string()),
        "win_external_pos reached decode_redraw and came back Unknown, so \
         the variant does not match what this engine sends"
    );

    let doc_path = view_oracle::workspace_root().join(DOC);
    let read = std::fs::read_to_string(&doc_path);
    assert!(
        read.is_ok(),
        "{} must be readable: {:?}",
        doc_path.display(),
        read.as_ref().err()
    );
    let doc = read.unwrap_or_default();

    assert_eq!(
        doc_fence(&doc, "### Captured event names, multigrid arm"),
        multigrid_names,
        "{DOC}'s multigrid event list has parted from the live stream"
    );
    assert_eq!(
        doc_fence(&doc, "### Captured event names, single-grid arm"),
        single_names,
        "{DOC}'s single-grid event list has parted from the live stream"
    );
    let mut undecoded = multigrid.undecoded.clone();
    undecoded.sort();
    assert_eq!(
        doc_fence(&doc, "### Names view's decoder has no variant for"),
        undecoded,
        "{DOC}'s undecoded-name list has parted from what decode_redraw answers"
    );
    assert_eq!(
        doc_fence(
            &doc,
            "### Captured event names, multigrid without `ext_messages`"
        ),
        probes.without_ext_messages,
        "{DOC}'s no-ext_messages event list has parted from the live stream"
    );
    let ground_truth: Vec<String> = multigrid
        .steps
        .iter()
        .map(|step| format!("{}: {}", step.label, step.windows))
        .collect();
    assert_eq!(
        doc_fence(&doc, "### Window-layout ground truth, multigrid arm"),
        ground_truth,
        "{DOC}'s window ground truth has parted from what nvim answers"
    );
}
