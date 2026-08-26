//! Live-nvim proof of the inline review's presentation: the two chunks
//! `EngineHandle::review_show` and `EngineHandle::review_clear` run, read
//! back out of a real editor.
//!
//! This is the half no unit test can reach. A unit test can pin what
//! crosses the wire; only nvim can answer whether an extmark landed on the
//! rows the payload named, whether the keys it set are reachable from the
//! buffer, whether the decoration left the text alone -- which is the
//! nvim-owns-the-text claim, stated as an assertion -- and whether a mark
//! naming a row the user has since deleted throws instead of drawing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

use view_core::msg::{BufferHandle, HunkMark, Msg, ReviewOpenTarget};
use view_core::native::mappings::review_keys;
use view_engine::process::{Engine, EngineConfig};

/// How long a notification produced by a fed key is waited for: two live
/// round trips (the key reaching nvim, the notification coming back),
/// widened for the load the run started under, because a cold nvim on a
/// loaded box is the slow part.
fn arrival() -> Duration {
    common::rpc_deadline_for(2)
}

const LINES: [&str; 7] = ["one", "two", "three", "four", "five", "six", "seven"];

/// A live nvim with a UI attached and its pump wired, so a key pressed in
/// the reviewed buffer is observable as the `Msg` the runtime loop sees.
///
/// The attach is not decoration: window layout, cursor placement and
/// `feedkeys` all need an nvim that has finished starting, which an
/// `--embed` connection does only once a UI arrives.
struct Session {
    engine: Engine,
    rx: Receiver<Msg>,
}

fn start() -> Session {
    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx): (SyncSender<Msg>, Receiver<Msg>) = std::sync::mpsc::sync_channel(256);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine.handle.ui_attach(80, 24).expect("attach ui");
    Session { engine, rx }
}

impl Session {
    fn lua(&self, chunk: &str, args: Vec<rmpv::Value>) -> rmpv::Value {
        self.engine
            .handle
            .request(
                "nvim_exec_lua",
                vec![rmpv::Value::from(chunk), rmpv::Value::Array(args)],
            )
            .expect("lua chunk")
    }

    fn strings(&self, chunk: &str, args: Vec<rmpv::Value>) -> Vec<String> {
        self.lua(chunk, args)
            .as_array()
            .expect("a list")
            .iter()
            .map(|v| v.as_str().expect("a string").to_owned())
            .collect()
    }

    /// A scratch buffer holding [`LINES`], shown in the current window.
    fn buffer(&self) -> BufferHandle {
        let handle = self
            .lua(
                "local buf = vim.api.nvim_create_buf(true, false)\n\
                 vim.api.nvim_buf_set_lines(buf, 0, -1, false, ...)\n\
                 return buf",
                vec![rmpv::Value::Array(
                    LINES.iter().map(|l| rmpv::Value::from(*l)).collect(),
                )],
            )
            .as_u64()
            .expect("a buffer handle");
        BufferHandle(handle)
    }

    fn show_in_current_window(&self, buf: BufferHandle) {
        self.lua(
            "vim.api.nvim_win_set_buf(0, ...)",
            vec![rmpv::Value::from(buf.0)],
        );
    }

    fn text(&self, buf: BufferHandle) -> String {
        self.lua(
            "return table.concat(vim.api.nvim_buf_get_lines(..., 0, -1, false), '\\n')",
            vec![rmpv::Value::from(buf.0)],
        )
        .as_str()
        .expect("buffer text")
        .to_owned()
    }

    /// Every extmark view's own namespace holds, one string per mark:
    /// `row:end_row:line_hl_group:sign_text:above:virt|lines`.
    fn decoration(&self, buf: BufferHandle) -> Vec<String> {
        self.strings(
            "local buf = ...
local ns = vim.api.nvim_create_namespace('view_review')
local out = {}
for _, m in ipairs(vim.api.nvim_buf_get_extmarks(buf, ns, 0, -1, { details = true })) do
  local d = m[4]
  local virt = {}
  for _, line in ipairs(d.virt_lines or {}) do
    local text = ''
    for _, chunk in ipairs(line) do
      text = text .. chunk[1] .. '/' .. tostring(chunk[2])
    end
    virt[#virt + 1] = text
  end
  out[#out + 1] = string.format('%d:%s:%s:%s:%s:%s', m[2], tostring(d.end_row),
    tostring(d.line_hl_group), tostring(d.sign_text), tostring(d.virt_lines_above),
    table.concat(virt, '|'))
end
return out",
            vec![rmpv::Value::from(buf.0)],
        )
    }

    /// Every normal-mode mapping local to `buf`, as `lhs -> rhs`.
    fn buffer_keys(&self, buf: BufferHandle) -> Vec<String> {
        self.strings(
            "local out = {}
for _, m in ipairs(vim.api.nvim_buf_get_keymap(..., 'n')) do
  out[#out + 1] = m.lhs .. ' -> ' .. (m.rhs or '')
end
return out",
            vec![rmpv::Value::from(buf.0)],
        )
    }

    /// Types `notation` and waits for nvim to have consumed it: the eval is
    /// the barrier, since a deferred request is answered only once the fed
    /// keys have run and anything they notified is already on the wire.
    fn press(&self, notation: &str) {
        self.engine.handle.feed_keys(notation).unwrap();
        self.engine.handle.eval_str("1").unwrap();
    }

    /// Forces every notify issued before this point to have been processed:
    /// nvim handles one channel's messages in order, so a request that has
    /// answered is a notify that has run.
    fn barrier(&self) {
        self.engine.handle.eval_str("1").unwrap();
    }

    fn show(&self, buf: BufferHandle, marks: &[HunkMark], cursor_row: u32, focus: bool) {
        self.engine
            .handle
            .review_show(buf, marks, cursor_row, focus, ReviewOpenTarget::Current)
            .unwrap();
        self.barrier();
    }
}

/// A replacement of one row, carrying the header rows the current hunk
/// draws.
fn replacement(row: u32, added: &[&str], header: &[&str]) -> HunkMark {
    HunkMark {
        row,
        end_row: row + 1,
        anchor: row,
        added: added.iter().map(|l| (*l).to_string()).collect(),
        stale: false,
        current: !header.is_empty(),
        header: header.iter().map(|l| (*l).to_string()).collect(),
    }
}

/// The whole presentation, read back out of nvim: the replaced row carries
/// the removal highlight, the proposal hangs off it as virtual lines in the
/// review's own add group, and the header sits above them on the current
/// hunk only.
#[test]
fn a_shown_review_decorates_the_rows_it_replaces_and_nothing_else() {
    let s = start();
    let buf = s.buffer();

    s.show(
        buf,
        &[
            replacement(
                1,
                &["TWO"],
                &["hunk 1/2 -- <leader>ha accept", "]c next  <leader>hq leave"],
            ),
            replacement(5, &["SIX"], &[]),
        ],
        1,
        false,
    );

    let marks = s.decoration(buf);
    assert_eq!(
        marks.len(),
        4,
        "two hunks, a highlight and a body each: {marks:?}"
    );
    assert_eq!(
        marks[0], "1:1:ViewReviewRemoved:nil:nil:",
        "the replaced row is highlighted where it really is: {marks:?}"
    );
    assert_eq!(
        marks[1],
        "1:nil:nil:\u{25b6} :false:hunk 1/2 -- <leader>ha accept/ViewReviewHeader|\
         ]c next  <leader>hq leave/ViewReviewHeader|+TWO/ViewReviewAdded",
        "the current hunk carries every header row, the sign, and the proposed line: {marks:?}"
    );
    assert_eq!(marks[2], "5:5:ViewReviewRemoved:nil:nil:");
    assert_eq!(
        marks[3], "5:nil:nil:nil:false:+SIX/ViewReviewAdded",
        "every other hunk shows its lines and no header: {marks:?}"
    );
}

/// What the user sees, not what the extmark stores: a hunk's `old_range`
/// is half-open and nvim's `end_row` is inclusive, so a range that reads
/// correct in the mark can still paint the untouched row below the hunk as
/// deleted. Only a rendered cell answers that, and this is the surface
/// that *is* the feature.
#[test]
fn the_deletion_highlight_stops_at_the_last_row_the_hunk_replaces() {
    let s = start();
    let buf = s.buffer();
    s.show_in_current_window(buf);

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);

    // screen rows, not buffer rows: the header and the proposed line are
    // drawn between the replaced row and the row that follows it
    let attrs = s.strings(
        "vim.cmd('redraw')
local out = {}
for row = 1, 5 do
  out[#out + 1] = tostring(vim.fn.screenattr(row, 3))
end
return out",
        vec![],
    );

    assert_ne!(
        attrs[1], attrs[0],
        "the row the hunk replaces is painted: {attrs:?}"
    );
    assert_eq!(
        attrs[4], attrs[0],
        "the row below the hunk is not the proposal's to paint: {attrs:?}"
    );
}

/// Both chunks answer for a buffer that is gone -- a show racing a
/// `:bwipeout`, a clear after one -- rather than raising inside nvim,
/// which is what each chunk's validity guard is for. The raise itself is
/// unobservable from inside the editor (a notification's error reaches
/// nvim's log, not `:messages`, not `v:errmsg`, and not this connection),
/// so what is asserted here is the observable half: the session carries on,
/// and the next review still lands.
#[test]
fn a_wiped_buffer_is_neither_drawn_nor_an_error() {
    let s = start();
    let buf = s.buffer();
    s.show_in_current_window(s.buffer());
    s.lua(
        "vim.api.nvim_buf_delete(..., { force = true })",
        vec![rmpv::Value::from(buf.0)],
    );

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, true);
    s.engine.handle.review_clear(buf).unwrap();
    s.barrier();

    let live = s.buffer();
    s.show(live, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);
    assert_eq!(
        s.decoration(live).len(),
        2,
        "the review after the wiped one lands, so neither call left the session disturbed"
    );
    assert_eq!(s.buffer_keys(live).len(), review_keys().len());
}

/// A pure insertion replaces no row, so it highlights nothing and draws
/// its lines above the row it is inserted before.
#[test]
fn a_pure_insertion_draws_above_the_row_and_highlights_nothing() {
    let s = start();
    let buf = s.buffer();

    s.show(
        buf,
        &[HunkMark {
            row: 3,
            end_row: 3,
            anchor: 3,
            added: vec!["inserted".to_string()],
            stale: false,
            current: false,
            header: Vec::new(),
        }],
        3,
        false,
    );

    assert_eq!(
        s.decoration(buf),
        vec!["3:nil:nil:nil:true:+inserted/ViewReviewAdded"]
    );
}

/// A hunk the buffer has moved under is drawn in the stale group rather
/// than the removed one: the rows are still the hunk's, but what they hold
/// is no longer what the proposal was computed against.
#[test]
fn a_stale_hunk_is_drawn_in_the_stale_group() {
    let s = start();
    let buf = s.buffer();

    let mut mark = replacement(1, &["TWO"], &[]);
    mark.stale = true;
    s.show(buf, &[mark], 1, false);

    assert!(
        s.decoration(buf)[0].contains("ViewReviewStale"),
        "{:?}",
        s.decoration(buf)
    );
}

/// The nvim-owns-the-text rule, as an assertion: the whole decoration is
/// presentation, and a review that has drawn every hunk has written no
/// byte of the buffer -- nor left it modified for the user to save.
#[test]
fn decorating_a_buffer_changes_neither_its_text_nor_its_modified_flag() {
    let s = start();
    let buf = s.buffer();
    let state = |s: &Session| {
        (
            s.text(buf),
            s.lua(
                "local buf = ...\n\
                 return { vim.api.nvim_buf_get_changedtick(buf), vim.bo[buf].modified }",
                vec![rmpv::Value::from(buf.0)],
            ),
        )
    };
    let before = state(&s);

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, true);

    assert_eq!(
        state(&s),
        before,
        "an extmark is not an edit: neither the text, nor b:changedtick, nor the modified \
         flag the user would be asked to save may move"
    );
}

/// The one-authority claim: an extmark shifts with the user's own edit
/// natively, which is why `rebase` stays the only thing tracking where a
/// hunk is and the decoration is never re-issued for an edit elsewhere.
#[test]
fn an_edit_above_a_hunk_carries_its_decoration_down_with_it() {
    let s = start();
    let buf = s.buffer();
    s.show(buf, &[replacement(4, &["FIVE"], &[])], 4, false);

    s.lua(
        "vim.api.nvim_buf_set_lines(..., 0, 0, false, { 'inserted', 'inserted' })",
        vec![rmpv::Value::from(buf.0)],
    );

    let marks = s.decoration(buf);
    assert!(
        marks[0].starts_with("6:6:"),
        "two rows inserted above must move the mark by two: {marks:?}"
    );
}

/// A row the payload names can already be past the end of the buffer by
/// the time the notify arrives. `strict = false` is what keeps that from
/// throwing -- and a throw would abandon the rest of the chunk, including
/// the keys, leaving a decorated buffer nothing can act on.
#[test]
fn a_mark_past_the_end_of_a_shrunk_buffer_still_installs_the_keys() {
    let s = start();
    let buf = s.buffer();

    s.lua(
        "vim.api.nvim_buf_set_lines(..., 1, -1, false, {})",
        vec![rmpv::Value::from(buf.0)],
    );
    s.show(buf, &[replacement(40, &["late"], &["hunk 1/1"])], 40, false);

    assert_eq!(
        s.buffer_keys(buf).len(),
        review_keys().len(),
        "the keys must survive a mark the buffer can no longer hold"
    );
}

/// The keys are the review's, they are the buffer's alone, and the review
/// takes every one of them back when it ends -- a buffer left holding one
/// would answer for a review that is gone.
#[test]
fn the_review_keys_are_buffer_local_and_leave_with_the_review() {
    let s = start();
    let buf = s.buffer();
    let other = s.buffer();

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);

    let keys = s.buffer_keys(buf);
    assert_eq!(keys.len(), review_keys().len(), "{keys:?}");
    assert!(
        keys.iter()
            .any(|k| k.starts_with("\\ha -> ") && k.contains("'view_invoke', 'review', 'accept'")),
        "the right-hand side is a readable rpcnotify, as `:map` shows it: {keys:?}"
    );
    assert!(
        s.buffer_keys(other).is_empty(),
        "a review decorates one buffer and claims keys in no other"
    );

    s.engine.handle.review_clear(buf).unwrap();
    s.barrier();

    assert!(s.buffer_keys(buf).is_empty(), "{:?}", s.buffer_keys(buf));
    assert!(s.decoration(buf).is_empty(), "{:?}", s.decoration(buf));
}

/// A review's keys land on mappings the user's own config already made --
/// gitsigns claims `]c`, `[c` and `<leader>hR` buffer-locally in every file
/// it attaches to -- and the migration contract is that those come back
/// when the review ends. A right-hand-side mapping returns with its text,
/// and a Lua-callback mapping returns still callable, which is the case
/// that decides where the bookkeeping can live. The redraw in the middle is
/// the trap: a second show must not save the review's own keys as if they
/// were the user's.
#[test]
fn a_review_hands_back_the_buffer_local_maps_it_displaced() {
    let s = start();
    let buf = s.buffer();
    s.show_in_current_window(buf);
    s.lua(
        "local buf = ...
vim.keymap.set('n', ']c', ':echo \"gitsigns\"<CR>', { buffer = buf, desc = 'user next hunk' })
vim.keymap.set('n', '\\\\hR', function() vim.g.view_test_hit = 'user' end, { buffer = buf })",
        vec![rmpv::Value::from(buf.0)],
    );

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);
    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);

    let during = s.buffer_keys(buf);
    assert_eq!(
        during.len(),
        review_keys().len(),
        "the review claims the user's keys rather than adding to them: {during:?}"
    );
    assert!(
        during
            .iter()
            .any(|k| k.starts_with("]c -> ") && k.contains("'review', 'next'")),
        "while the review is open its own verb answers: {during:?}"
    );

    s.engine.handle.review_clear(buf).unwrap();
    s.barrier();

    let after = s.buffer_keys(buf);
    assert_eq!(
        after.len(),
        2,
        "only the two the user made survive the review: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|k| k.starts_with("]c -> ") && k.contains("gitsigns")),
        "the displaced right-hand side is the user's again: {after:?}"
    );
    s.press("\\hR");
    assert_eq!(
        s.lua("return vim.g.view_test_hit", vec![]).as_str(),
        Some("user"),
        "a restored lua mapping is callable, not just listed"
    );
}

/// Both calls are total and idempotent: showing twice leaves one review's
/// worth of decoration, and clearing a buffer that was never shown is not
/// an error.
#[test]
fn a_second_show_replaces_the_first_and_a_clear_without_one_is_harmless() {
    let s = start();
    let buf = s.buffer();

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/2"])], 1, false);
    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/2"])], 1, false);

    assert_eq!(
        s.decoration(buf).len(),
        2,
        "a redraw replaces what it drew before rather than stacking on it"
    );

    let untouched = s.buffer();
    s.engine.handle.review_clear(untouched).unwrap();
    s.barrier();
    assert!(s.buffer_keys(untouched).is_empty());
}

/// The point of the whole mechanism: a key pressed in the reviewed buffer
/// arrives as the same `Msg::FeatureInvoke` the `:View` command produces,
/// with no panel involved anywhere in the path.
#[test]
fn pressing_a_review_key_in_the_buffer_invokes_the_verb() {
    let s = start();
    let buf = s.buffer();
    s.show_in_current_window(buf);
    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);

    s.press("\\ha");

    let deadline = std::time::Instant::now() + arrival();
    let mut verb = None;
    while verb.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match s.rx.recv_timeout(left) {
            Ok(Msg::FeatureInvoke { feature, verb: v }) if feature == "review" => verb = Some(v),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(verb.as_deref(), Some("accept"));
}

/// `focus` is the difference between the review coming to the user and the
/// review repainting under them. It takes the cursor to the hunk; without
/// it, a status change must never move a reader who has scrolled away.
#[test]
fn focus_takes_the_cursor_to_the_hunk_and_a_repaint_leaves_it_alone() {
    let s = start();
    let buf = s.buffer();
    s.show_in_current_window(buf);

    s.show(buf, &[replacement(4, &["FIVE"], &["hunk 1/1"])], 4, true);
    assert_eq!(
        s.lua("return vim.api.nvim_win_get_cursor(0)[1]", vec![]),
        rmpv::Value::from(5),
        "focus lands the cursor on the hunk's own row"
    );

    s.lua("vim.api.nvim_win_set_cursor(0, { 1, 0 })", vec![]);
    s.show(buf, &[replacement(4, &["FIVE"], &["hunk 1/1"])], 4, false);
    assert_eq!(
        s.lua("return vim.api.nvim_win_get_cursor(0)[1]", vec![]),
        rmpv::Value::from(1),
        "a repaint must not yank a reader who has scrolled away"
    );
}

/// A file no window is showing lands in the window the user is in, which
/// is what the picker and the tree already do for "show me this file".
#[test]
fn an_unopened_file_lands_in_the_current_window() {
    let s = start();
    let buf = s.buffer();
    let windows_before = s.lua("return #vim.api.nvim_list_wins()", vec![]);

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, true);

    assert_eq!(
        s.lua("return vim.api.nvim_get_current_buf()", vec![]),
        rmpv::Value::from(buf.0)
    );
    assert_eq!(
        s.lua("return #vim.api.nvim_list_wins()", vec![]),
        windows_before,
        "the current-window target rearranges nothing"
    );
}

/// Configured for a split, the same case keeps whatever the user was
/// reading beside the proposal.
#[test]
fn open_target_split_leaves_the_users_own_file_beside_the_proposal() {
    let s = start();
    let buf = s.buffer();
    let reading = s.lua("return vim.api.nvim_get_current_buf()", vec![]);

    s.engine
        .handle
        .review_show(
            buf,
            &[replacement(1, &["TWO"], &["hunk 1/1"])],
            1,
            true,
            ReviewOpenTarget::Split,
        )
        .unwrap();
    s.barrier();

    assert_eq!(
        s.lua("return vim.api.nvim_get_current_buf()", vec![]),
        rmpv::Value::from(buf.0),
        "the proposal is where the cursor is"
    );
    assert_eq!(
        s.lua("return #vim.api.nvim_list_wins()", vec![]),
        rmpv::Value::from(2)
    );
    assert!(
        s.strings(
            "local out = {}
for _, w in ipairs(vim.api.nvim_list_wins()) do
  out[#out + 1] = tostring(vim.api.nvim_win_get_buf(w))
end
return out",
            vec![]
        )
        .contains(&reading.as_u64().expect("a buffer handle").to_string()),
        "the file the user was reading is still on screen"
    );
}

/// A window already showing the file is where the review is drawn,
/// whatever `open_target` says: `open_target` decides only the case where
/// nothing has the file open at all.
#[test]
fn a_file_a_window_already_shows_is_never_moved_or_split() {
    let s = start();
    let buf = s.buffer();
    s.show_in_current_window(buf);
    let windows_before = s.lua("return #vim.api.nvim_list_wins()", vec![]);

    s.engine
        .handle
        .review_show(
            buf,
            &[replacement(1, &["TWO"], &["hunk 1/1"])],
            1,
            true,
            ReviewOpenTarget::Split,
        )
        .unwrap();
    s.barrier();

    assert_eq!(
        s.lua("return #vim.api.nvim_list_wins()", vec![]),
        windows_before
    );
}

/// The colorscheme shape the review's own groups are derived from: the
/// four diff groups exactly as `nvim_get_hl` reports them under
/// dracula.nvim -- foreground-only, two of them `reverse` -- over a
/// `Normal` background to blend against.
const DRACULA_SHAPED: &str = "\
vim.cmd('hi Normal guifg=#F8F8F2 guibg=#282A36')
vim.cmd('hi DiffDelete guibg=NONE guifg=#FF5555 gui=reverse')
vim.cmd('hi DiffText guibg=NONE guifg=#8BE9FD gui=reverse')
vim.cmd('hi DiffChange guibg=NONE guifg=#FFB86C gui=NONE')
vim.cmd('hi DiffAdd guibg=NONE guifg=#50FA7B gui=NONE')";

impl Session {
    /// `name`'s own resolved colors, as `bg/fg/reverse`.
    fn group(&self, name: &str) -> String {
        self.lua(
            "local hl = vim.api.nvim_get_hl(0, { name = ..., link = false })
local hex = function(c) return c and string.format('%06x', c) or 'nil' end
return string.format('%s/%s/%s', hex(hl.bg), hex(hl.fg), tostring(hl.reverse == true))",
            vec![rmpv::Value::from(name)],
        )
        .as_str()
        .expect("a group's colors")
        .to_owned()
    }

    /// One painted cell of the rendered screen, as
    /// `char:bg/fg/reverse`: the attributes the UI was handed, rather than
    /// the extmark that asked for them.
    fn cell(&self, row: u64, col: u64) -> String {
        self.lua(
            "vim.cmd('redraw')
local cell = vim.api.nvim__inspect_cell(1, ...)
local a = cell[2] or {}
local hex = function(c) return c and string.format('%06x', c) or 'nil' end
return string.format('%s:%s/%s/%s', cell[1], hex(a.background), hex(a.foreground),
  tostring(a.reverse == true))",
            vec![rmpv::Value::from(row), rmpv::Value::from(col)],
        )
        .as_str()
        .expect("a painted cell")
        .to_owned()
    }
}

/// A colorscheme's diff groups are the review's *source* of color, never
/// the groups it paints with. Under dracula's shape those groups are
/// foreground-only and `reverse`, and a `line_hl_group` naming one fills
/// the whole row with a solid block of it -- the bright-red rows a user
/// reported instead of their colorscheme. What the review paints with
/// instead is a fifth of that color over `Normal`'s background, carrying
/// no attribute the diff group had.
#[test]
fn a_reverse_video_diff_group_becomes_a_subtle_background_not_a_solid_block() {
    let s = start();
    s.lua(DRACULA_SHAPED, vec![]);
    let buf = s.buffer();
    s.show_in_current_window(buf);

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);

    assert_eq!(s.group("ViewReviewRemoved"), "53333c/nil/false");
    assert_eq!(s.group("ViewReviewStale"), "534641/nil/false");
    assert_eq!(s.group("ViewReviewAdded"), "305444/50fa7b/false");
    assert_eq!(s.group("ViewReviewHeader"), "3c505e/8be9fd/false");
    assert_eq!(s.group("ViewReviewSign"), "nil/8be9fd/false");

    // buffer row 1 is screen row 1, and column 2 is its first text cell
    // once the current hunk's sign has claimed the two before it
    assert_eq!(
        s.cell(1, 2),
        "t:53333c/nil/false",
        "the row the hunk replaces is painted in the blend, keeps whatever \
         foreground its own syntax gave it, and is not reversed"
    );
    assert_eq!(
        s.cell(2, 2),
        "h:3c505e/8be9fd/false",
        "the header's virtual line is the blend too, with the diff group's own \
         color as its text"
    );
}

/// The other half of the same claim: a colorscheme that defines its diff
/// groups as backgrounds -- nvim's own default among them -- already chose
/// what a diffed row sits on, so that background is taken as it stands
/// rather than averaged with anything. Only a group with no background of
/// its own is converted from its foreground.
#[test]
fn a_background_defined_diff_group_is_taken_verbatim() {
    let s = start();
    s.lua(
        "vim.cmd('hi Normal guifg=#F8F8F2 guibg=#000000')
vim.cmd('hi DiffDelete guifg=NONE guibg=#500000 gui=NONE')",
        vec![],
    );
    let buf = s.buffer();

    s.show(buf, &[replacement(1, &["TWO"], &[])], 1, false);

    assert_eq!(
        s.group("ViewReviewRemoved"),
        "500000/nil/false",
        "the scheme's own diff background, unblended, with nothing taken from \
         the foreground"
    );
}

/// The text a review draws over its own background is never that
/// background. A colorscheme defining a diff group by background alone --
/// `habamax`, which the pinned nvim ships, is one
/// (`hi DiffAdd guifg=NONE guibg=#273923`) -- leaves the proposal no color
/// of its own, and answering that with the group's background paints
/// `+BETA` in exactly the color of the row it sits on. `Normal`'s
/// foreground is what the row's own text would have used, so it is what
/// the proposal uses.
#[test]
fn a_background_only_diff_group_draws_its_text_in_normals_foreground() {
    let s = start();
    s.lua(
        "vim.cmd('hi Normal guifg=#C7C7C7 guibg=#1C1C1C')
vim.cmd('hi DiffAdd guifg=NONE guibg=#273923 gui=NONE')
vim.cmd('hi DiffText guifg=NONE guibg=#0F4F4F gui=NONE')",
        vec![],
    );
    let buf = s.buffer();
    s.show_in_current_window(buf);

    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);

    assert_eq!(s.group("ViewReviewAdded"), "273923/c7c7c7/false");
    assert_eq!(s.group("ViewReviewHeader"), "0f4f4f/c7c7c7/false");
    assert_eq!(s.group("ViewReviewSign"), "nil/c7c7c7/false");
    for name in ["ViewReviewAdded", "ViewReviewHeader"] {
        let resolved = s.group(name);
        let (bg, rest) = resolved.split_once('/').expect("bg/fg/reverse");
        let (fg, _) = rest.split_once('/').expect("fg/reverse");
        assert_ne!(
            bg, fg,
            "{name} would draw its text in the color of the row it sits on"
        );
    }
    assert_eq!(
        s.cell(3, 2),
        "+:273923/c7c7c7/false",
        "the proposed line is legible where it is painted, not the row's own color"
    );
}

/// A colorscheme themeing a diff group with neither a foreground nor a
/// background is the branch a minimal or hand-rolled scheme takes, and the
/// review still has to read on it. `Normal`'s own foreground is what it
/// converts instead, so the row is marked in the one color the scheme is
/// certain to have.
#[test]
fn a_diff_group_with_no_color_at_all_falls_back_to_normals_foreground() {
    let s = start();
    s.lua(
        "vim.cmd('hi Normal guifg=#F8F8F2 guibg=#282A36')
vim.cmd('hi DiffDelete guifg=NONE guibg=NONE gui=NONE ctermfg=NONE ctermbg=NONE cterm=NONE')",
        vec![],
    );
    let buf = s.buffer();

    s.show(buf, &[replacement(1, &["TWO"], &[])], 1, false);

    assert_eq!(
        s.group("ViewReviewRemoved"),
        "52535c/nil/false",
        "a fifth of #F8F8F2 over #282A36: the review reads under a scheme that \
         themes no diff group at all"
    );
}

/// The groups hold resolved colors rather than a link, so a scheme
/// switched while a review is open would keep the old scheme's tint on the
/// rows without something to re-derive them. That something is the
/// `ColorScheme` autocmd the show installs in the review's own augroup.
#[test]
fn a_colorscheme_switched_under_an_open_review_re_derives_its_groups() {
    let s = start();
    s.lua(DRACULA_SHAPED, vec![]);
    let buf = s.buffer();
    s.show_in_current_window(buf);
    s.show(buf, &[replacement(1, &["TWO"], &["hunk 1/1"])], 1, false);
    let before = s.group("ViewReviewRemoved");

    s.lua("vim.cmd('colorscheme blue')", vec![]);

    let after = s.group("ViewReviewRemoved");
    assert_ne!(
        before, after,
        "the review's groups follow the colorscheme the user is now in"
    );
    // the value is written out rather than recomputed from the scheme that
    // is now loaded: an assertion that ran the chunk's own arithmetic would
    // reproduce an error in it instead of catching one. `blue` themes
    // `DiffDelete` with a background (#af5faf), so what the review takes
    // from it is that background itself
    assert_eq!(
        after, "af5faf/nil/false",
        "and they are that scheme's own diff color, read the way any scheme's is"
    );
}
