# P5.5 media handoff + image viewing — one plan, in build order

Supersedes `.claude/plans/2026-08-09-p5_5-media.md` and `.claude/plans/2026-08-09-p5_5-image.md`. Suggested path: `.claude/plans/2026-10-03-p5_5-media-image.md`. Add an INDEX.md row and mark both originals superseded.

Verified at HEAD afc3d5d1. Every path and line below was read in the tree. Where the recon disagrees with the tree, the tree wins, and the differences are listed under "Recon corrections".

## 1. Design

### 1.1 Types and signatures

**view-core (pure)**

```rust
// crates/view-core/src/native/open.rs (new)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenKind { Text, Image, Media }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTarget { pub path: PathBuf, pub line: Option<u64> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenConfig {
    pub image: bool,            // [image] enabled
    pub media: bool,            // [media] enabled
    pub player: PathBuf,        // [media] player, derived "mpv"
    pub tree_hover: Duration,   // [ui.surfaces.tree] hover_preview_ms
}
impl Default for OpenConfig { /* image=false, media=false until T5/T9 flip */ }

/// Matches the extension case-insensitively. No content sniffing.
/// Text is returned when the class is disabled.
pub fn classify(path: &Path, cfg: &OpenConfig) -> OpenKind;
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
const MEDIA_EXTS: &[&str] = &["mp4","mkv","webm","mov","avi","m4v","mp3","flac","ogg","opus","wav","m4a"];
```

```rust
// crates/view-core/src/update/open.rs (new)
/// The one open dispatch. Text goes to the engine. Image goes to the image surface.
/// All Media targets go to ONE player run. A remote session sends everything to Text.
pub(super) fn dispatch(model: &mut Model, targets: Vec<OpenTarget>) -> Vec<Effect>;
```

```rust
// msg.rs additions
Msg::OpenPaths { paths: Vec<PathBuf> }                    // CLI only
Msg::TerminalReturned(LoanOutcome)
Msg::ImageDecoded { generation: u64, image: Result<Arc<DecodedImage>, String> }
Msg::TreeHoverDue { generation: u64 }

Effect::LendTerminal(TerminalLoan)
Effect::DecodeImage { generation: u64, path: PathBuf, purpose: ImageUse }
Effect::ScheduleTreeHover { generation: u64, after: Duration }

RpcCall::OpenFileAt { path: String, line: u64 }           // beside OpenFile (msg.rs:2450)

pub enum TerminalLoan {
    Shell,                                                 // nvim `suspend` (Ctrl-Z)
    Media { player: PathBuf, paths: Vec<PathBuf>, kitty_graphics: bool },
}
pub enum LoanOutcome {
    Resumed,
    PlayerExited { code: Option<i32> },
    PlayerMissing { player: String },
    PlayerFailed { error: String },
}
pub enum ImageUse { Viewer, PickerPreview, TreeHover }

// model/image.rs (new)
pub struct DecodedImage { pub id: u32 /* 24-bit */, pub width: u32, pub height: u32, pub rgba: Arc<[u8]> }
// PartialEq compares id only. Debug prints the id and dimensions only.

// events.rs
UiEvent::Suspend
// caps.rs
TermCaps { …, kitty_graphics: bool }  +  pub fn with_kitty_graphics(self, on: bool) -> Self
// geometry.rs
NativeSurface::Image   (ALL: [_; 5])
// view-surface
LayerKind::Image(ImageRef)  // ImageRef { image: Arc<DecodedImage>, fit: Rect }
```

**view-engine**

```rust
const OPEN_FILE_AT_CHUNK: &str; // local path, line = ...; nvim_cmd edit (magic file/bar false);
                                // pcall(vim.api.nvim_win_set_cursor, 0, {line, 0})
pub fn open_file_at(&self, path: &str, line: u64) -> Result<(), EngineError>;
pub fn file_operands(args: &[OsString]) -> Vec<(usize, &OsStr)>; // visibility only (process.rs:2468)
```

**view-tui (the only crate that touches the terminal)**

```rust
impl Term {
    pub fn suspend(&mut self) -> io::Result<()>;            // restore_bytes subset, cooked mode
    pub fn resume(&mut self) -> io::Result<()>;             // raw, enter_bytes, re-push kitty kbd, reset diff state
    pub fn suspend_until_continued(&mut self) -> io::Result<()>; // suspend, SIGTSTP to self, resume
}
fn suspend_to<W: Write>(out: &mut W, rows: u16, kitty_kbd_pushed: bool) -> io::Result<()>;
fn resume_to<W: Write>(out: &mut W, kitty_kbd: bool) -> io::Result<()>;
impl InputSource { pub fn set_lent(&mut self, lent: bool); pub fn is_lent(&self) -> bool; }
```

**view-native**

```rust
// crates/view-native/src/image.rs (new)
pub fn decode(path: &Path, cancelled: impl Fn() -> bool) -> Result<DecodedImage, DecodeError>;
#[derive(Debug, thiserror::Error)] pub enum DecodeError { TooLarge, Unsupported, Malformed(String), Io(io::Error), Cancelled }
```

**view (bin)**

```rust
// crates/view/src/on_path.rs (new; lifted from remote_guard.rs:595 unchanged)
pub enum Presence { … }
pub fn resolve_client(program: &Path, path_var: Option<&OsStr>, path_ext: Option<&OsStr>) -> Presence;

// crates/view/src/media.rs (new)
pub fn player_command(player: &Path, kitty_graphics: bool, paths: &[PathBuf]) -> Command;
// --vo=kitty | --vo=tct, then "--", then paths
pub(crate) fn begin(loan: TerminalLoan, term: &mut Term, input: &mut TermInput,
                    engine: &mut EngineSession, tx: &LoopSender) -> Option<Msg>;
pub(crate) fn finish(term: &mut Term, input: &mut TermInput, engine: &mut EngineSession) -> io::Result<()>;
```

### 1.2 Open dispatch data flow

```
 CLI argv ──main: cli.remote None?──yes──► file_operands(passthrough)
   │                                         │ classify each operand
   │                                         ├─ Text  ─► stays in passthrough (nvim opens it; respawn replays it)
   │                                         └─ Image/Media ─► stripped ─► InputHandles.opens
   │                                                                │
   └─ --remote ─► never classified                                  ▼
                                                     run(): pending.push(Msg::OpenPaths)
 tree <CR> (file)  ─► pop overlay / FocusPreviousWindow ─┐          │
 picker <CR>       ─► PickerClose ───────────────────────┤          │
                                                          ▼          ▼
                                   update/open.rs::dispatch(model, Vec<OpenTarget>)
                                     │ remote? ─► all Text
                    ┌────────────────┼──────────────────────────┐
                  Text             Image                      Media (all, one run)
                    │                │                          │
     Rpc OpenFile / OpenFileAt   open NativeSurface::Image   Effect::LendTerminal(Media{..})
     (engine owns the buffer)    + Effect::DecodeImage(Viewer)    │ executor: loans_tx.send
                                     │ executor: spawn_or_log      ▼
                                     │ view_native::image::decode  loop: drain loans ─► media::begin
                                     ▼                              │ absent ─► TerminalReturned(PlayerMissing)
                               Msg::ImageDecoded ─► model           │ else suspend, lend input, pause heartbeat,
                                     ▼                              │ waiter thread: spawn_tied mpv, wait
                               LayerKind::Image ─► paint/image.rs   ▼
                               (kitty placeholders | ▀ half-blocks) Msg::TerminalReturned(PlayerExited)
                                                                     ─► media::finish ─► notice when code != 0
```

### 1.3 Usage

```toml
# view.toml — every key optional; the values shown are the derived defaults
[image]
enabled = false        # off switch: images open as text buffers again
[media]
enabled = false        # off switch: videos open as buffers again
player = "mpv"         # looked up on PATH like the ssh client; never bundled

[ui.surfaces.image]
placement = "overlay"  # or "window": a window beside your code
anchor = "center"      # windowed default "right"
size = 80              # percent

[ui.surfaces.tree]
hover_preview_ms = 300 # 0 turns hover previews off
```

| Form | What it shows |
|---|---|
| `view pic.png` | the editor with the image viewer open over it |
| `view a.png b.jpg notes.md` | notes.md in the engine, the viewer on a.png; `n`/`p` step to b.jpg |
| `view talk.mp4` | mpv full-screen (kitty video where the terminal supports it, text-cell video otherwise); `q` returns to the editor |
| `view a.mp4 b.mkv` | one mpv run with a playlist of both |
| `view --remote h:pic.png` | sent to the remote nvim as text (never classified) |
| tree `<CR>` on an image / video | viewer / mpv |
| tree hover on an image for 300 ms | a preview floating to the right of the tree |
| picker `<Down>` `<C-n>` / `<Up>` `<C-p>` | moves the selection and refreshes the preview (images show pixels) |
| picker `<CR>` | opens the selection through the dispatch; a live-grep hit lands on its line |
| viewer `n` / `p` | next / previous image of the same open |
| viewer `q` / `<Esc>` | close (a windowed viewer's `<Esc>` goes back to the previous window, as the tree's does) |
| `<C-z>` in the editor | the shell; `fg` brings the editor back intact |
| no mpv installed | notice: "mpv not found on PATH; set [media] player" — the terminal is never handed over |
| `<C-c>` inside mpv | mpv quits; view survives |

## 2. Tasks

Ordering rules: new code goes in new modules; ceilings are model.rs 994, process.rs 988, paint.rs 983, update/surfaces.rs 953. A cross-crate signature change adds the new form beside the old, then switches callers. Every task leaves `task ci` green.

### T1 — Picker selection movement, `<CR>` accept, `RpcCall::OpenFileAt`

- **Files**
  - `view-core/src/native/picker.rs`: add `pub fn move_selection(&mut self, delta: isize)` (clamped, no wrap) and `pub fn selected_line(&self) -> Option<u64>` (LiveGrep only).
  - `view-core/src/update/route.rs`: in the picker arm (:337) intercept `<Down>`/`<C-n>`/`<Up>`/`<C-p>` (move, then `picker_preview_request`) and `<CR>` (pop, `PickerClose`, then open effects) before `edit_query`.
  - `view-core/src/update/surfaces.rs`: fix the doc on `picker_preview_request` (:25) that says no navigation exists. Comments only, so the line count is net zero.
  - `msg.rs`: add `RpcCall::OpenFileAt`.
  - `view-engine/src/nvim_api.rs`: add `OPEN_FILE_AT_CHUNK` and `open_file_at`, and add the chunk to the PUBLISHED list (~:4527).
  - `docs/tree-open-file-wire-capture.md`: add a fenced section with the marker "The Lua chunk under test, verbatim `OPEN_FILE_AT_CHUNK`:" plus a live capture.
  - `view/src/engine_ops.rs`: add `open_file_at` to the trait (:27) and all five impls (:331/:583/:837/:1119/:1464). Also add it to `view/src/clipboard.rs:1035`.
  - `view/src/runtime/executor.rs`: add an OpenFileAt arm beside :413.
- **Accept, temporarily:** in T1, `<CR>` emits `Rpc(OpenFile|OpenFileAt)` directly. T2 reroutes it through dispatch.
- **Tests**
  - `picker_down_moves_selection_and_requests_a_preview`: selected goes 0 to 1, and the effect list holds `PreviewBufferWindow` for item 1's path.
  - `picker_up_at_top_stays_at_zero`.
  - `picker_ctrl_n_and_ctrl_p_alias_down_and_up`.
  - `picker_enter_on_a_file_closes_and_opens_it`: `[PickerClose, Rpc(OpenFile{root/label})]`.
  - `picker_enter_on_a_grep_hit_opens_at_its_line`: `OpenFileAt{line}`.
  - `picker_enter_on_no_name_buffer_only_closes`.
  - `picker_enter_with_no_results_only_closes`.
  - `open_file_at_chunk_lands_the_cursor` (headless nvim test beside the open_file one): cursor equals `(line, 0)`, and a line past EOF does not error.
- **Pinning tests that must stay green:** `edit_query_ignores_multi_char_notation_but_still_bumps` (picker.rs:613; unchanged because navigation is intercepted upstream), `every_wire_capture_fence_matches_its_chunk_byte_for_byte`, and the existing `<Esc>` pre-check tests in route.rs.
- **Capture:** `scripts/dogfood/cap.sh --keys '<leader>ff' --keys 'main' --keys '<Down><Down>' out.txt -- .` under the real config shows the third row highlighted and its preview. A second run with `<CR>` shows the file open.
- **Performance:** key dispatch gains two string compares before `edit_query` when a picker is open. Nothing changes when it is closed.
- **Commit:** `feat(picker): move through results with arrows or Ctrl-N/Ctrl-P and open the selection with Enter`

### T2 — The shared open dispatch, with both classes off

- **Files**
  - new `view-core/src/native/open.rs` (`classify`, `OpenTarget`, `OpenConfig`).
  - new `view-core/src/update/open.rs` (`dispatch`; Image and Media arms are unreachable while both flags are false, but the code is present and tested with flags on).
  - `model.rs`: add the `pub open: OpenConfig` field (+2 lines, leaving 996).
  - `view-surface/src/cache.rs`: classify `open` as not a paint input.
  - `msg.rs`: add `Msg::OpenPaths`.
  - `update/surfaces.rs`: tree `<CR>` file branch (:1350-1379) calls `open::dispatch` with the windowed `FocusPreviousWindow` prefix kept. This is net zero lines because the RpcCall construction moves out.
  - `route.rs`: picker `<CR>` calls `open::dispatch`.
  - `view-engine/src/process.rs`: make `file_operands` `pub` (no line change).
  - `view/src/main.rs`: when `cli.remote.is_none()`, split passthrough. Classified Image/Media operands are removed from `cli.passthrough` *before* `engine_config` (:1132), so `respawn_config` (:649) never replays them.
  - `view/src/runtime.rs`: add `InputHandles.opens: Vec<PathBuf>` (:727); `run` seeds `pending` with `Msg::OpenPaths` when it is non-empty.
- **Tests**
  - `classify_is_case_insensitive_and_extension_only` (`A.PNG` is Image, `png` with no extension is Text, `.png` dotfile is Text).
  - `classify_returns_text_when_the_class_is_disabled`.
  - `dispatch_sends_text_to_the_engine`.
  - `dispatch_batches_every_media_target_into_one_loan`.
  - `dispatch_in_a_remote_session_sends_everything_to_text`.
  - `tree_enter_on_a_file_still_opens_it_through_the_dispatch` (the existing tree `<CR>` tests keep their expected effects byte for byte).
  - main.rs: `media_operands_never_reach_the_respawn_config` (source-order test beside :1990-2140) and `a_remote_path_is_never_classified`.
  - runtime.rs: `cli_opens_are_seeded_before_the_first_wait`.
- **Pinning:** all tree `<CR>` tests in update/surfaces.rs; `stdin_operands` tests; `every_model_field_is_a_paint_input_or_named_here`; main source-order tests.
- **Capture:** `cap.sh out.txt -- README.md` is unchanged from before (classes are off).
- **Performance:** not on key, grid or paint paths. One extension compare per opened path.
- **Commit:** `refactor(open): route the command line, the tree and the picker through one open path`

### T3 — Lending the terminal: `Term::suspend`/`resume`, input lending, Ctrl-Z

- **Files**
  - `view-tui/src/terminal.rs`: add `suspend`, `resume`, `suspend_until_continued`, `suspend_to`, `resume_to`.
    - Suspend writes ESU, the kitty kbd pop when pushed, `\x1b[0 q`, Show, DisableMouseCapture, DisableBracketedPaste and LeaveAlternateScreen, then leaves raw mode.
    - Resume enters raw mode, writes `enter_bytes`, re-pushes kitty kbd when `caps.kitty_kbd`, and resets `shadow = Shadow::new()`, `last_cursor`, `cursor_shown`, `last_mouse_reporting = Some(false)`, `last_cursor_shape` and `last_offset`.
    - It does not touch `RESTORED`, installs no panic hook, and keeps the stderr redirect.
    - `suspend_until_continued` calls `rustix::process::kill_process(getpid(), Signal::TSTP)`. Add the `process` feature to view-tui's rustix.
    - On non-unix it is a no-op plus a log line.
    - terminal.rs is 464 lines, so it has room; if it would cross 1000, move the code to `terminal/loan.rs`.
  - `view-tui/src/input.rs`:
    - Store the `repeat` flag and add a `lent: Arc<AtomicBool>`.
    - `FATAL_SIGNALS` becomes `[SIGHUP, SIGTERM, SIGINT, SIGQUIT]`.
    - While lent, a SIGINT/SIGQUIT is swallowed: the fatal value is cleared and `repeat` is reset. SIGHUP/SIGTERM still end view.
    - `drain`, `has_buffered` and `next_deadline` are inert while lent.
    - Windows: a process-wide `INPUT_LENT`, and `spawn_input_thread` uses `crossterm::event::poll(50ms)` and skips `read()` while lent.
  - `view/src/wake.rs`: `poll_readiness` leaves the tty fd out while lent.
  - `view/src/runtime.rs`:
    - Paint gate (:1414) becomes `model.dirty && !backlog && !input.is_lent()`.
    - A loans channel is drained beside `drain_pass_handoffs` (:1342).
  - new `view/src/media.rs` (`begin`/`finish`, Shell only in T3).
  - `executor.rs`: add `with_loans(tx)` and the `Effect::LendTerminal` arm (send only, never blocks).
  - `events.rs`: add `UiEvent::Suspend`. `view-engine/src/ui_events.rs` decodes `"suspend"`. Update the vlog.rs:2537 census test. `update/ui_event.rs` emits `LendTerminal(Shell)`. `native/speculate.rs:806` lists it.
  - `main.rs`: the startup executor (:1592) gets `.with_loans(loans_tx)`.
- **Signals, and why this design:** mpv keeps ISIG, so Ctrl-C/Ctrl-\ inside mpv deliver SIGINT/SIGQUIT to view's process group. Today SIGINT is fatal (input.rs:351), so Ctrl-C in the player would kill the editor.
  - Swallowing while lent is the smallest safe fix.
  - Rejected: a separate process group plus `tcsetpgrp`. Taking the terminal back needs SIGTTOU ignored or blocked, which needs `unsafe` (denied workspace-wide), and a caught handler loops on ERESTARTSYS.
- **Tests**
  - `suspend_writes_the_teardown_without_marking_restored`: bytes contain `\x1b[?1049l` and the kitty pop; `RESTORED` is still false.
  - `resume_reenters_and_repushes_kitty_keyboard_when_the_terminal_has_it`.
  - `resume_forces_a_full_frame` (draw after resume emits every cell).
  - `a_lent_terminal_swallows_sigint_and_sigquit`.
  - `sighup_still_ends_view_while_lent`.
  - `drain_is_inert_while_lent`.
  - `suspend_ui_event_decodes`.
  - `suspend_event_lends_the_shell`.
  - `the_paint_gate_holds_while_lent` (source walk beside `the_per_pass_backstop_reaches_the_loop`).
- **Pinning:** `the_teardown_escapes_are_written_once_per_process` (terminal.rs:1684); `every_wakeup_the_struct_declares_reaches_the_deadline_fold`; `the_per_pass_backstop_reaches_the_loop`; the vlog census test (expected to flip `suspend` from Unknown, which is intended).
- **Capture:** `cap.sh --keys '<C-z>' --settle 1 out.txt -- README.md` shows the shell prompt. Sending `fg` then brings back a full, uncorrupted frame. That needs two cap.sh runs, or a manual tmux send-keys in the same session; record both screens.
- **Performance:** paint gate gains one bool load. Poll set gains one branch. Key dispatch is unchanged.
- **Commit:** `feat(editor): Ctrl-Z suspends to the shell and fg brings the editor back intact`

### T4 — The `kitty_graphics` capability probe (every tier)

- **Files**
  - `view-core/src/model/caps.rs`: add `kitty_graphics: bool` and `with_kitty_graphics`. `from_probe` keeps its signature and sets the field false. Add-beside, then switch.
  - `view-tui/src/tiers.rs`:
    - `QUERY_KITTY_GRAPHICS = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"`, written before the DA1 fence.
    - `scan_replies` (:921) recognizes `ESC _ G i=31;OK ESC \`. Any other `i=31;…` reply means false.
    - A 5th `REGISTER` row (`[CapabilityRow; 5]`, :252) with `Fallback::Unanswered`.
    - `caps_for_override(Full)` sets it, so "every boolean" stays true.
  - `view-tui/src/input.rs`: the late guard (:599) carries the bit into `CapsUpgraded`.
  - `view-oracle/src/pty.rs`: add a `KITTY_GRAPHICS` answer to `FULL_TIER`.
  - `docs/terminal-probe-wire-capture.md`: new live captures with `scripts/capture-terminal-probe.sh probe` on kitty, ghostty, WezTerm (mbp) and tmux. Tmux is expected to stay silent.
- **Tests**
  - `scan_replies_reads_a_kitty_graphics_ok`.
  - `a_graphics_error_reply_reads_false`.
  - `the_graphics_query_precedes_the_da1_fence`.
  - The existing register tests then cover the new row: `every_termcaps_field_has_a_register_row` (:2297), `every_rows_probe_is_a_query_the_batch_actually_writes` (:2355), `every_live_capture_resolves_to_what_the_doc_reads_it_as` (:1861, with the new captures), and `the_full_tier_answers_every_capability_view_tui_probes_for` (pty.rs:1368).
- **Pinning:** `a_terminals_tier_never_reaches_its_frame` (native_overlay_goldens.rs:727). The cache `Inputs` holds `TermCaps` whole, so the new bit invalidates for free.
- **Capture:** the probe hex dumps above, plus `cap.sh out.txt -- README.md` in tmux showing an unchanged frame.
- **Performance:** about 40 bytes added to the one batched probe; no extra round trip, and it stays inside `shell_visible_ms`.
- **Commit:** `feat(terminal): detect whether the terminal can show pictures`

### S1 — Spec amendment: the capability register (lands with T4)

- `.claude/specs/2026-07-17-view-design.md`:
  - After :599, add the row: `| kitty_graphics | Kitty graphics query action (ESC _ G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \); reply ESC _ G i=31;OK ESC \ | — | false | image pixels (half-block cells when false); mpv --vo=kitty vs --vo=tct |`.
  - :608: "All four ride the one batched startup probe" becomes "All five ride the one batched startup probe".
  - :618: add `kitty_graphics` gates image pixels and the player's video output.
  - Decision log: one dated line.
- **Commit:** `docs(spec): add the picture capability to the terminal register`

### T5 — Media handoff

- **Files**
  - new `view/src/on_path.rs`: `resolve_client` + `Presence` + `spellings` + `is_file` move out of `remote_guard.rs` (:595-651) unchanged. remote_guard re-imports them, and its tests (:775-832) move with the code.
  - `view/src/media.rs`:
    - `player_command`.
    - The `begin` Media arm: if `resolve_client` reports absent, return `Msg::TerminalReturned(PlayerMissing)` without suspending. Otherwise suspend, call `input.set_lent(true)` and `engine.heartbeat.pause()` (heartbeat.rs:570), then use `spawn_or_log("media-player", …)` to run `view_proc::spawn_tied_to_this_process` with stdio on `/dev/tty` (unix) or inherited (Windows). Wait, then `LoopMsgOutbox::route(tx, Msg::TerminalReturned(PlayerExited{code}))`.
    - `finish`: `term.resume()`, `set_lent(false)`, `heartbeat.resume()` (:588, which forgives outstanding probes).
  - `scripts/check-style.sh`: add `crates/view/src/media.rs 1` to `TIED_SPAWN_SITES` (:603).
  - `view-core/src/update/open.rs`: the Media arm emits `LendTerminal(Media{player, paths, kitty_graphics: model.caps.kitty_graphics})`. The update arm for `TerminalReturned`:
    - `PlayerMissing` → `model.engine.record_native_notice("mpv not found on PATH; set [media] player".into(), false)`.
    - `PlayerFailed` → notice.
    - `PlayerExited{Some(c)}` with c ∉ {0, 4} → a notice with the code (mpv exit 4 means quit by signal or user).
    - `model.dirty = true`.
  - New config handling:
    - `view-native/src/config/keys.rs` gains rows `media.enabled` and `media.player` (both derived).
    - `config/resolve.rs` resolves `OpenConfig.media`/`player` (the `enabled` default flips to **true** here).
    - Add the keys to `view.toml.example`.
    - `view-core/src/native/mappings.rs`: `REGISTRY_EXEMPT_FEATURES` grows to `[_; 4]` with `{id: "media", off_switch: "media.enabled = false"}`. Amend the `is_reachable_feature` doc sentence: "A feature lands in this list in the same commit that gives it a mapping row, or, for a feature with no key of its own, the commit that gives it its off switch."
    - `main.rs` sets `model.open` from resolved config beside `set_layouts` (:1350).
  - New fixtures: `scripts/test-fixtures/fake-mpv` (bash 3.2) and `fake-mpv.cmd`. The fake writes its argv NUL-separated to `$FAKE_MPV_ARGV`, prints `FAKE-MPV PLAYING`, reads one byte from the tty, and exits `${FAKE_MPV_EXIT:-0}`.
- **Tests**
  - `player_command_uses_kitty_video_when_the_terminal_shows_pictures` (argv `--vo=kitty -- a.mp4`).
  - `player_command_falls_back_to_text_cells` (`--vo=tct`).
  - `player_command_ends_options_before_paths` (path `-x.mp4` stays after `--`).
  - `several_media_operands_go_to_one_run`.
  - `a_missing_player_raises_a_notice_and_keeps_the_terminal` (with an injected player path that does not exist; `Term` is never suspended, checked through a recording writer).
  - `the_player_exit_resumes_the_heartbeat` (FakeOps).
  - `a_nonzero_player_exit_raises_a_notice`.
  - `media_enabled_false_opens_videos_as_text`.
  - `resolve_client` tests (moved).
  - `the_media_feature_names_its_off_switch` (mappings test list).
  - Tests pass a player path. PATH is never mutated.
- **Pinning:** `check_tied_spawns`; `the_registry_and_the_example_document_the_same_keys` (keys.rs:375); `every_key_has_a_derived_default` (resolve.rs:1550); the mappings tests that read `REGISTRY_EXEMPT_FEATURES` (mappings.rs:735+, view-native report.rs, config.rs:1270, chords.rs:938); remote_guard tests.
- **Capture:** with `player = "<repo>/scripts/test-fixtures/fake-mpv"` in a real config copy, `cap.sh --settle 1 out.txt -- clip.mp4` shows `FAKE-MPV PLAYING`. A second capture after one key shows the editor.
- **Performance:** not on key, grid or paint paths. The paint gate check was added in T3.
- **Commit:** `feat(media): opening a video hands the terminal to mpv and takes it back when mpv exits`

### T6 — Media acceptance + real-mpv capture

- **Files**
  - new `scripts/acceptance/media.sh` (tmux, fake mpv, bash 3.2). Legs:
    1. CLI `view a.mp4 b.mkv` → one argv record holding both paths after `--`.
    2. Tree `<CR>` on a video.
    3. Picker `<CR>` on a video.
    4. `player` pointing at a missing file → the notice text appears and no fake output.
    5. `<C-c>` while the fake is running → view's pane is still alive and the frame comes back.
    6. `FAKE_MPV_EXIT=2` → the exit notice.
  - Taskfile `acceptance:` list (:206-229) gains it.
- **Real capture (coordinator installs mpv on dev-linux):**
  - Generate the clip with `ffmpeg -f lavfi -i testsrc=size=320x240:rate=10 -t 5 clip.mp4`.
  - Run `cap.sh --settle 2 out.txt -- clip.mp4` under the real config. Inside tmux the probe says false, so expect `--vo=tct`: `▀` cells and mpv's `AV:` status line.
  - Then `--keys q` → the editor frame is back.
  - Record both in the T6 commit body's evidence note.
- **Commit:** `test(media): acceptance for the mpv handoff from the command line, tree and picker`

### T7 — Image decode in view-native

- **Files**
  - `crates/view-native/Cargo.toml`: add `png`, `zune-jpeg`, `gif`, `image-webp`.
    - These are the decoders the `image` crate itself uses, so the dependency graph is a strict subset of `image`'s, without its encoders, rayon or format zoo.
    - No deny.toml or cargo-audit config exists. The policy is `scripts/audit-deps.sh`, so add confinement rows there (the four crates are allowed only in view-native).
  - new `view-native/src/image.rs`:
    - Refuse files over 64 MiB, or headers over 16384 px per side.
    - GIF: first frame composed onto the logical screen.
    - Box downscale to fit 1600×1600.
    - RGBA8 out.
    - `cancelled()` is checked between rows and chunks.
    - The 24-bit id comes from a process-wide `AtomicU32` masked to 0xFFFFFF, skipping 0.
  - new `view-core/src/model/image.rs` (`DecodedImage`). It is re-exported from model.rs (+1 line, leaving 997).
  - `msg.rs`: add `Effect::DecodeImage`, `Msg::ImageDecoded` and `ImageUse`.
  - `executor.rs`: add a DecodeImage arm copying the PickerPreviewFallbackWindow pattern (:770-792): `spawn_or_log`, a per-purpose `Arc<AtomicU64>` latest (three fields), and `tx.send`.
  - Fixtures `crates/view-native/tests/fixtures/image/` with tiny PNG, JPEG, GIF (2 frames), WebP (lossy + lossless) and a truncated PNG, generated once with magick and committed.
- **Tests**
  - `decodes_png_jpeg_gif_and_webp_to_rgba`: dimensions plus one pixel from each.
  - `a_gif_decodes_its_first_frame_only`.
  - `a_truncated_png_is_malformed_not_a_panic`.
  - `an_oversized_header_is_refused_before_allocation`.
  - `a_large_image_is_downscaled_to_fit`.
  - `cancellation_stops_a_decode`.
  - `ids_never_repeat_and_never_exceed_24_bits`.
  - Executor: `a_superseded_decode_never_reaches_the_model`.
- **Pinning:** audit-deps confinement rows; `every_model_field_is_a_paint_input_or_named_here` (no model field yet).
- **Performance:** decode runs off the paint thread; nothing changes on key, grid or paint paths.
- **Commit:** `feat(image): decode PNG, JPEG, GIF and WebP pictures`

### T8 — Make room in model.rs

- Move `OverlayKind` and its impls (model.rs ~:2343-2410) to new `view-core/src/model/overlay.rs`, re-exported. This is behaviour-neutral.
- **Pinning:** the full view-core suite.
- **Commit:** `refactor(model): move overlay kinds into their own module`

### T9 — The image surface: viewer, half-block cells, config

- **Files**
  - `native/geometry.rs`: add `NativeSurface::Image`.
    - `ALL: [_; 5]`; name "image"; `dotted_table` gives `ui.surfaces.image`.
    - `default_for` is Center, overlay, 80; `default_windowed_anchor` is Right.
  - Grow every `[_; 4]` keyed by the surface: `placement.rs::advance_ring` (:134) and `ResolvedConfig.surfaces` (resolve.rs ~:208).
  - Walk all 23 `NativeSurface::ALL` uses and every `match` on the enum: route.rs, update/mod.rs, placement.rs, window_status.rs, update/surfaces.rs (`reopen_after_restart` :615-700), notice.rs, update/resize.rs, model/focus.rs, view-tui paint/panes.rs, model/resize.rs, view-surface lib.rs, and view-native config/surfaces.rs and config/resolve.rs.
  - new `view-core/src/native/image_view.rs`: `ImageViewState { generation, paths: Vec<PathBuf>, index, image: Option<Arc<DecodedImage>>, error: Option<String> }`.
  - `model/overlay.rs`: add `OverlayKind::Image(ImageViewState)`.
  - new `view-core/src/update/image.rs`:
    - Opening; the `ImageDecoded` arm (a generation match, else drop).
    - Keys `n`/`p`/`q`/`<Esc>`. Windowed `<Esc>` → `FocusPreviousWindow`, as the tree does.
    - Windowed placement uses `OpenNativeWindow{surface: Image, …}`. This is a native scratch window the engine lays out; the image bytes never enter nvim.
  - `update/open.rs`: the Image arm opens the viewer with every image target of this open.
  - `view-surface/src/lib.rs`: add `LayerKind::Image(ImageRef)`. The layer is built for the viewer rect, sized from cells at a 1:2 cell aspect.
  - new `view-tui/src/paint/image.rs`: half-block `▀` (fg = top pixel, bg = bottom pixel, truecolor or nearest-256) via area-average sampling into the rect. paint.rs's `composite_into` gets one arm, `LayerKind::Image(r) => image::paint(area, r, caps, buf)` (+1 line, leaving 984).
  - Config:
    - keys.rs gains `image.enabled` and `ui.surfaces.image.*` (the surface rows come for free from ALL).
    - Add both to `view.toml.example`.
    - `REGISTRY_EXEMPT_FEATURES` gets `{id: "image", off_switch: "image.enabled = false"}` (`[_; 5]`).
    - The `image.enabled` default flips to true here.
  - `view-surface/src/cache.rs`: the overlay already counts as an input through `had_overlays`/the overlay view; add the image id to the compared view.
- **Tests**
  - `opening_an_image_shows_the_viewer_and_requests_a_decode`.
  - `n_and_p_step_through_the_images_of_one_open`.
  - `q_closes_the_viewer`.
  - `a_windowed_viewer_escape_returns_to_the_previous_window`.
  - `a_stale_decode_is_dropped`.
  - `a_decode_error_shows_in_the_viewer_not_as_a_panic`.
  - `half_blocks_carry_top_and_bottom_pixels` (2×2 quadrant fixture → exact SGR colors).
  - `image_layer_fits_inside_its_rect_at_a_one_to_two_cell_aspect`.
  - `the_image_surface_round_trips_overlay_and_window` (placement ring).
  - `image_enabled_false_opens_pictures_as_text`.
  - Goldens in `view-oracle/tests/native_overlay_goldens.rs` for the viewer at Basic/Standard; paint fixtures spell `kitty_graphics: false`.
- **Pinning:** `every_model_field_is_a_paint_input_or_named_here`; the surface-ring and placement tests; `the_registry_and_the_example_document_the_same_keys`; `every_key_has_a_derived_default`; `a_terminals_tier_never_reaches_its_frame`.
- **Capture:** `cap.sh --ansi out.ansi -- scripts/acceptance/fixtures/image/quadrants.png` under the real config shows `▀` cells with 38;2/48;2 in the four quadrant colors.
- **Performance:** paint gains one match arm. Without an image layer the cost is zero. With one, sampling is O(rect cells), and only damaged frames repaint.
- **Commit:** `feat(image): open a picture from the command line, the tree or the picker in a viewer`

### T10 — Sharp pixels: kitty Unicode placeholders

- **Files**
  - `view-tui/src/paint/image.rs`: when `caps.kitty_graphics`, cells hold U+10EEEE plus the row and column diacritics.
    - The 297-entry table goes in new `view-tui/src/paint/kitty_diacritics.rs`.
    - The fg truecolor carries the 24-bit id.
  - `view-tui/src/terminal.rs`:
    - Add `Term.transmitted: HashSet<u32>`.
    - `queue_frame` (:934) transmits a not-yet-sent id: `a=t,f=32,s=W,v=H,i=ID,q=2,U=1,m=1` in 4096-byte base64 chunks. The base64 encoder is ~20 lines local, so no crate.
    - It places with `a=p,U=1,i=ID,c=COLS,r=ROWS,q=2`.
    - Close and `suspend` delete with `a=d,d=I,i=ID` and clear the set; `resume` lets it retransmit.
  - Placeholders are ordinary cells, so the diff engine and the shadow handle them. Only transmit and delete are side channels.
- **Tests**
  - `placeholder_cells_encode_row_column_and_id`.
  - `an_image_is_transmitted_once_across_frames`.
  - `transmission_is_chunked_at_4096`.
  - `suspend_deletes_transmitted_images`.
  - `without_the_capability_no_graphics_bytes_are_written`.
- **Pinning:** T9 half-block tests; `the_teardown_escapes_are_written_once_per_process`.
- **Capture:** T13 leg A (tmux cannot show graphics).
- **Performance:** placeholder cells cost the same as any glyph. Transmission happens once per image, and the frame that carries it is larger once.
- **Risk:** WezTerm may answer the query but not draw placeholders. The mbp WezTerm leg decides. If it fails, add a terminal-name refusal in the probe read and record it in the wire-capture doc.
- **Commit:** `feat(image): sharp pictures on kitty, ghostty and other terminals that show them`

### T11 — Picker preview for images

- **Files**
  - `update/surfaces.rs::picker_preview_request` (:25):
    - An Image-class path emits `DecodeImage{purpose: PickerPreview}` instead of `PreviewBufferWindow`.
    - A Media path emits no request, and the pane says "video — Enter plays it".
    - Net +3 lines, leaving 956 (under 1000). If that would cross, put the branch in update/image.rs.
  - `picker.rs` (or `native/picker/` if its own ceiling bites): `preview_image: Option<Arc<DecodedImage>>`, plus a generation match in the `ImageDecoded` arm.
  - The preview pane rect gets a `LayerKind::Image` layer.
- **Tests**
  - `moving_onto_an_image_requests_a_picture_preview`.
  - `moving_onto_a_video_requests_nothing`.
  - `moving_off_an_image_drops_its_late_decode`.
- **Pinning:** T1 picker tests and the `refresh_preview` tests.
- **Capture:** `cap.sh --ansi --keys '<leader>ff' --keys 'quadrants' out.ansi -- scripts/acceptance/fixtures/image` shows half-blocks in the preview pane.
- **Performance:** key dispatch gains one classify per selection move.
- **Commit:** `feat(picker): preview pictures in the picker`

### T12 — Tree hover preview

- **Files**
  - `native/tree.rs`: add `hover: Option<(u64, Arc<DecodedImage>)>` and `hover_generation`.
  - `update/surfaces.rs::tree_key`: `<Down>`/`<Up>` (:1330-1343) emit `ScheduleTreeHover{generation, after: model.open.tree_hover}` when the new selection is an image. 0 ms means off. That is +2 lines; if over budget, delegate to update/image.rs.
  - `msg.rs`: add `Effect::ScheduleTreeHover` and `Msg::TreeHoverDue`.
  - The executor reuses the toast timer pattern (`toast_timer` sender, :72).
  - `TreeHoverDue` with a matching generation emits `DecodeImage{TreeHover}`. The layer floats to the right of the tree rect, clipped to the screen.
  - Config: `ui.surfaces.tree.hover_preview_ms` (derived 300) goes in keys.rs and `view.toml.example`.
- **Tests**
  - `resting_on_an_image_schedules_a_hover`.
  - `moving_before_the_delay_cancels_it`.
  - `hover_preview_ms_zero_never_schedules`.
  - `a_hover_closes_when_the_tree_closes`.
- **Pinning:** tree navigation tests and the T5/T9 config pins.
- **Capture:** `cap.sh --ansi --keys '<leader>e' --keys '<Down>' --settle 1 out.ansi -- scripts/acceptance/fixtures/image`.
- **Performance:** tree key dispatch gains one classify plus one timer effect.
- **Commit:** `feat(tree): rest on a picture in the tree to preview it`

### T13 — Image acceptance (two concrete legs)

- New `scripts/acceptance/image.sh` (bash 3.2), added to the Taskfile `acceptance:` list. Fixture `scripts/acceptance/fixtures/image/quadrants.png`: 64×64, with red, green, blue and white quadrants.
- **Leg A — kitty under xvfb** (follows docs/terminal-probe-wire-capture.md's xvfb recipe):
  1. Run `xvfb-run -s '-screen 0 1280x800x24' kitty --dump-commands -o font_size=10 -e target/release/view quadrants.png` with the output to a log, then wait for settle.
  2. `import -window root shot.png`.
  3. Assert the log holds a graphics command with `U=1` and no `ENOENT`/`EINVAL` reply.
  4. `magick shot.png -crop …` samples the four quadrant centres of the viewer rect, and each must be within ±8 per channel of its fixture colour.
  5. Quit with `q` via `kitten @ send-text` when remote control is enabled with `-o allow_remote_control=yes`.
- **Leg A2:** the same as A but running view inside tmux inside kitty. Assert no graphics commands are logged and the samples show the half-block colours (looser tolerance, ±24).
- **Leg B — half-block through cap.sh:** `scripts/dogfood/cap.sh --ansi --size 80x24 out.ansi -- quadrants.png` under the real config. Assert `▀` count > 0, and that `38;2;255;0;0` and `48;2;…` sequences appear for each quadrant colour.
- macOS (mbp): run leg A against ghostty and WezTerm by hand, and record the results in the evidence note.
- **Commit:** `test(image): acceptance for sharp and half-block pictures`

### S2 — Spec amendment: rows 806 and 807 (lands with T9)

- :806 (Image), replacing "the engine keeps the buffer". Proposed text:

  > Opening an image from the command line, the tree or the picker shows it in the image surface (overlay by default, a window by config); nvim never loads the image bytes, and a windowed image is a native scratch window the engine lays out like the tree's. Pixels use kitty Unicode placeholders when `kitty_graphics` is true and half-block cells otherwise. Engine-side opens (`:e pic.png`, plugin pickers) open text buffers.

- :807 (Media): name the three entry points, `--vo=kitty`/`--vo=tct` from `kitty_graphics`, `player` resolved on PATH and never bundled, terminal loaned and reclaimed, and the plan path.
- Decision log :1204-1205: one dated line each.
- **Commit:** `docs(spec): pictures open in their own view and engine opens stay text`

### T14 — README, docs, plans index

- README:
  - Remove the "Image viewing" and "Media handoff" rows from "### Next" (:160-164).
  - Add Features paragraphs (wording checked against `PROSE_MECHANISM`: no paint, surface, passthrough, tier, composited, rpc):
    > **Pictures in the editor.** Open a PNG, JPEG, GIF or WebP from the command line, the tree or the picker, or rest on one in the tree to preview it. Sharp on kitty, ghostty and other terminals that show pictures, blocky elsewhere.
    >
    > **Videos in mpv.** `view talk.mp4`, or a video picked in the tree or the picker, hands the terminal to `mpv` and takes it back when you quit. Install `mpv` yourself; view tells you when it is missing.
  - :68-70 becomes: "The tree, the agent panel, the command palette, the notifications and the picture viewer each open … One key moves all five between the two."
  - The "**Panes for anything.**" Next row stays (it covers more than these two).
  - Add "Ctrl-Z suspends to the shell" to "Everyday details".
- `docs/keymaps.md`: add "## The picture viewer" (`n`, `p`, `q`, `<Esc>`) and picker rows (`<Down>`/`<C-n>`, `<Up>`/`<C-p>`, `<CR>`) as `| key | does |` tables.
- `.claude/plans/INDEX.md`: add a row for this plan and mark both originals superseded.
- **Commit:** `docs: pictures, videos and picker navigation in the README and keymaps`

## 3. Conflicts

| Conflict | Where | Resolution |
|---|---|---|
| "the engine keeps the buffer" vs no buffer for images | spec:806 | S2 amends it to the surface reading; Fork F1 keeps the buffer-keyed option open |
| Exempt-feature doc rule requires a mapping row | mappings.rs `is_reachable_feature` doc | T5 amends the sentence |
| SIGINT is fatal, and mpv keeps ISIG | input.rs:351 | T3 adds SIGQUIT to the list and swallows both while lent |
| `[_; 4]` arrays keyed by surface | placement.rs:134, resolve.rs ~:208, README "all four" | T9 grows them to 5; T14 changes the prose |
| model.rs is at 994 | model.rs | T2 +2, T7 +1, then T8 frees ~70 lines before T9 |
| paint.rs is at 983 | paint.rs | one arm only (+1); the logic lives in paint/image.rs |
| update/surfaces.rs is at 953 | surfaces.rs | net 0 in T1/T2, +3 in T11, +2 in T12; overflow goes to update/image.rs |
| Probe batch change needs new captures | spec C1, wire-capture doc | T4 records kitty, ghostty, WezTerm and tmux |
| A spawn site outside the allowed list | check-style.sh:603 | T5 adds a `media.rs 1` row |
| `OpenFile` callers across five impls | engine_ops.rs | add-beside `open_file_at`; `open_file` is untouched |
| Respawn would replay media operands | main.rs:649 | T2 strips them before `engine_config` |
| Windows has no tty fd to drop from poll | view-tui `spawn_input_thread` :1263 | T3 adds `INPUT_LENT` and a poll(50ms) skip |

## 4. Dropped from the originals, and why

- **Blocking `Flow::MediaHandoff`:** it stalls message draining. Replaced by a waiter thread plus `Msg::TerminalReturned`.
- **Reusing `TerminalGuard` for the loan:** `enter_raw` installs a panic hook on every call and `restore` sets `RESTORED`. Replaced by `Term::suspend`/`resume`.
- **`model.heartbeat`:** the heartbeat belongs to Engine (`process.rs:1015`). Paused through the session.
- **`[native]` registry rows and the supersedes field:** decision 5. Exempt rows replace them.
- **Spawning `mpv --version` to detect mpv:** `resolve_client` does it without a process.
- **A PATH-prepend fake:** tests inject the player path.
- **Gating on `Tier::Full`:** C1 says a tier gates nothing. The `kitty_graphics` bit gates instead.
- **Decode in core, and the `image` crate:** core stays pure; four direct decoders are a strict subset of `image`'s graph.
- **A bespoke full-screen overlay:** NativeSurface::Image reuses placement, the ring and windowing.
- **`[native] tree_hover_delay_ms`:** it moved to `[ui.surfaces.tree] hover_preview_ms`.
- **The claim that no spec edit is needed:** S1 and S2.
- **WebP out of scope:** `image-webp` is cheap.
- **`toast::route` for the notice:** `record_native_notice` exists.
- **Composite arms that produce bytes, and PNG `f=100` passthrough:** paint stays cells-only; transmission lives in `Term`.
- **An `OpenSource` enum:** no consumer branches on it.
- **Machine-local exit-checklist items:** not reproducible.

## 5. Forks

### F1 — Engine-side opens (`:e pic.png`, a telescope pick)

The spec does not require this: :807 names exactly three entry points, and :806 as amended in S2 states that engine opens stay text. So it is a fork, not a task.

**A (recommended now): nothing.** `:e pic.png` shows bytes, as today.

```
:e pic.png      → a text buffer of PNG bytes (unchanged)
view pic.png    → the viewer
```

**B: a buffer-keyed hand-off with no plugin-specific code.**
- Add one autocmd to `REGISTER_BRIDGE_CHUNK` (nvim_api.rs:705, augroup `view_bridge`): `BufReadCmd *.png,*.jpg,…,*.mp4,…`.
- It runs `rpcnotify(chan, 'view_bridge', 'open', fname)` and wipes the buffer.
- In view this decodes to `Msg::OpenPaths` and goes through the same dispatch.
- Any plugin that ends in `:edit` is covered, with no plugin names in code.
- Costs:
  - The pattern list must be kept in step with `classify`. It would be generated from the same tables, so a fence test pins it.
  - A new wire capture.
  - A remote session needs the notify ignored.
  - `:w` on such a buffer is impossible, which is fine because the buffer is wiped.

```
:e pic.png             → the viewer; the alternate buffer is unchanged
<telescope pick> a.mp4 → mpv
```

Recommendation: ship A, and take B as its own small task once the dispatch has shipped and been used. B needs nothing beyond what T2 builds.

### F2 — Exit code 4 from mpv

mpv exits 4 when quit by a signal (Ctrl-C), which a user does on purpose. Recommendation: no notice for 0 or 4. The alternative is to show any non-zero code.

## 6. Recon corrections

- The `ReplyRecorder` impl that also needs `open_file_at` is at `crates/view/src/clipboard.rs:1035`, not :831.
- `stdin_operands` is at process.rs:2452. `file_operands` is at :2468 and is private; making it `pub` changes visibility only.
- "All four ride the one batched startup probe" is at spec:608, not :610.
- The picker test `edit_query_ignores_multi_char_notation_but_still_bumps` (picker.rs:613) does not need to change. Navigation is intercepted in route.rs before `edit_query`.
- The recon missed that Ctrl-C/Ctrl-\ inside mpv reach view as SIGINT/SIGQUIT, and that SIGINT is fatal today (input.rs:351). T3 addresses it.
- The `REGISTRY_EXEMPT_FEATURES` doc rule ("same commit that gives it a mapping row") conflicts with decision 5. T5 amends it.
- `advance_ring` (placement.rs:134) and `ResolvedConfig.surfaces` are fixed `[_; 4]` arrays and must grow with NativeSurface::Image. The recon treated adding the surface as additive only.
- No deny.toml or cargo-audit config exists. Dependency policy is `scripts/audit-deps.sh` (inside `task ci`).
- Ctrl-Z needs rustix's `process` feature in view-tui (`kill_process`, `getpid`, `Signal::TSTP`, verified in rustix 1.1.4).

### Critical Files for Implementation
- /opt/repos/view/crates/view-core/src/update/route.rs
- /opt/repos/view/crates/view-tui/src/terminal.rs
- /opt/repos/view/crates/view-tui/src/tiers.rs
- /opt/repos/view/crates/view/src/runtime.rs
- /opt/repos/view/crates/view-core/src/native/geometry.rs


## 7. Coordinator rulings (2026-10-03)

- **F1 is built as T15, after T13:** option B, the `BufReadCmd` hand-off
  in `REGISTER_BRIDGE_CHUNK`, with the pattern list generated from
  `classify`'s tables and pinned by a fence test. It registers a pattern
  only when the class is enabled and no other `BufReadCmd` autocmd already
  matches that pattern at `VimEnter`, so a config that handles pictures
  itself keeps doing so. A remote session ignores the notify. S2's last
  sentence becomes "Engine-side opens (`:e pic.png`, any plugin that ends
  in `:edit`) reach the same dispatch."
  Commit: `feat(open): :e on a picture or a video opens it the way the tree does`
- **F2:** no notice for mpv exit 0 or 4.
