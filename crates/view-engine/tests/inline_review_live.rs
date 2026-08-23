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

use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

use view_core::msg::{BufferHandle, HunkMark, Msg, ReviewOpenTarget};
use view_core::native::mappings::review_keys;
use view_engine::process::{Engine, EngineConfig};

/// How long a notification produced by a fed key is waited for. Generous
/// because a cold nvim on a loaded box is the slow part.
const ARRIVAL: Duration = Duration::from_secs(10);

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

/// A replacement of one row, carrying the header the current hunk draws.
fn replacement(row: u32, added: &[&str], header: Option<&str>) -> HunkMark {
    HunkMark {
        row,
        end_row: row + 1,
        anchor: row,
        added: added.iter().map(|l| (*l).to_string()).collect(),
        stale: false,
        current: header.is_some(),
        header: header.map(str::to_owned),
    }
}

/// The whole presentation, read back out of nvim: the replaced row carries
/// the deletion highlight, the proposal hangs off it as virtual lines in
/// nvim's own add group, and the header sits above them on the current
/// hunk only.
#[test]
fn a_shown_review_decorates_the_rows_it_replaces_and_nothing_else() {
    let s = start();
    let buf = s.buffer();

    s.show(
        buf,
        &[
            replacement(1, &["TWO"], Some("hunk 1/2 -- <leader>ha accept")),
            replacement(5, &["SIX"], None),
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
        marks[0], "1:2:DiffDelete:nil:nil:",
        "the replaced row is highlighted where it really is: {marks:?}"
    );
    assert_eq!(
        marks[1], "1:nil:nil:\u{25b6} :false:hunk 1/2 -- <leader>ha accept/DiffText|+TWO/DiffAdd",
        "the current hunk carries the header, the sign, and the proposed line: {marks:?}"
    );
    assert_eq!(marks[2], "5:6:DiffDelete:nil:nil:");
    assert_eq!(
        marks[3], "5:nil:nil:nil:false:+SIX/DiffAdd",
        "every other hunk shows its lines and no header: {marks:?}"
    );
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
            header: None,
        }],
        3,
        false,
    );

    assert_eq!(
        s.decoration(buf),
        vec!["3:nil:nil:nil:true:+inserted/DiffAdd"]
    );
}

/// A hunk the buffer has moved under is drawn in nvim's change group
/// rather than its delete group: the rows are still the hunk's, but what
/// they hold is no longer what the proposal was computed against.
#[test]
fn a_stale_hunk_is_drawn_in_the_change_group() {
    let s = start();
    let buf = s.buffer();

    let mut mark = replacement(1, &["TWO"], None);
    mark.stale = true;
    s.show(buf, &[mark], 1, false);

    assert!(
        s.decoration(buf)[0].contains("DiffChange"),
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

    s.show(buf, &[replacement(1, &["TWO"], Some("hunk 1/1"))], 1, true);

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
    s.show(buf, &[replacement(4, &["FIVE"], None)], 4, false);

    s.lua(
        "vim.api.nvim_buf_set_lines(..., 0, 0, false, { 'inserted', 'inserted' })",
        vec![rmpv::Value::from(buf.0)],
    );

    let marks = s.decoration(buf);
    assert!(
        marks[0].starts_with("6:7:"),
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
    s.show(
        buf,
        &[replacement(40, &["late"], Some("hunk 1/1"))],
        40,
        false,
    );

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

    s.show(buf, &[replacement(1, &["TWO"], Some("hunk 1/1"))], 1, false);

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

/// Both calls are total and idempotent: showing twice leaves one review's
/// worth of decoration, and clearing a buffer that was never shown is not
/// an error.
#[test]
fn a_second_show_replaces_the_first_and_a_clear_without_one_is_harmless() {
    let s = start();
    let buf = s.buffer();

    s.show(buf, &[replacement(1, &["TWO"], Some("hunk 1/2"))], 1, false);
    s.show(buf, &[replacement(1, &["TWO"], Some("hunk 1/2"))], 1, false);

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
    s.show(buf, &[replacement(1, &["TWO"], Some("hunk 1/1"))], 1, false);

    s.press("\\ha");

    let deadline = std::time::Instant::now() + ARRIVAL;
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

    s.show(buf, &[replacement(4, &["FIVE"], Some("hunk 1/1"))], 4, true);
    assert_eq!(
        s.lua("return vim.api.nvim_win_get_cursor(0)[1]", vec![]),
        rmpv::Value::from(5),
        "focus lands the cursor on the hunk's own row"
    );

    s.lua("vim.api.nvim_win_set_cursor(0, { 1, 0 })", vec![]);
    s.show(
        buf,
        &[replacement(4, &["FIVE"], Some("hunk 1/1"))],
        4,
        false,
    );
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

    s.show(buf, &[replacement(1, &["TWO"], Some("hunk 1/1"))], 1, true);

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
            &[replacement(1, &["TWO"], Some("hunk 1/1"))],
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
            &[replacement(1, &["TWO"], Some("hunk 1/1"))],
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
