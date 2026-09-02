#!/usr/bin/env bash
#
# Visual acceptance: every native surface a user reaches, driven through a
# real terminal against a real colorscheme, and read back cell by cell with
# the escape sequences still attached.
#
# The defect class this exists for is the one no unit test in this tree can
# see. A paint test asserts on a `ratatui::Buffer` -- resolved styles,
# already correct by the time the assertion reads them -- and never on the
# bytes crossterm writes for them. A capability gate that took the
# attributes-only branch under ssh, a chrome cell that patched instead of
# replacing what the grid left beneath it, a prompt row that painted the
# head of an input while the cursor typed past its edge: all three shipped
# green suites and all three are visible in one `capture-pane -e`. So this
# leg reads what the terminal was actually told, and it reads it through a
# colorscheme, because an unthemed session cannot tell an overlay that owns
# its cells from one that never had to.
#
# An acceptance assertion's expected color is read from the live scheme by
# probe, never from a config's text. Every background the assertions turn
# on comes back from a headless nvim started under the very XDG environment
# the driven session will use, resolved through `nvim_get_hl` -- and, for
# the groups a review is drawn with, through `REVIEW_SHOW_CHUNK`'s own
# `derive`, lifted out of the engine source and run rather than
# re-implemented here, so there is one arithmetic and not two. Reading a
# `#rrggbb` out of a colorscheme file with `sed` could only ever assert the
# single scheme this repo ships (and cost a BSD-`sed` word-boundary
# footnote to do it); a probe asserts whichever scheme the run was pointed
# at, transparent ones included -- a group with no background of its own
# reads back as `-`, the terminal default, which is what a capture shows
# for it.
#
# What it is pointed at: `VIEW_SWEEP_CONFIG`, an XDG_CONFIG_HOME directory
# holding `nvim/` (default: the themed fixture), and `VIEW_SWEEP_DATA`, an
# XDG_DATA_HOME whose installed plugin set the run borrows. Borrowing links
# in that home's `nvim/lazy` and nothing else, and neuters the module that
# installs, so a plugin manager finds every plugin already installed and has
# no way to write one -- while every byte anything else writes (a tool
# installer's downloads, a session file, a project history) lands in this
# run's own scratch instead of in the home it came from.
#
# The entry points driven are read out of `DEFAULT_MAPS` on the same terms
# as the colors: a feature that gains a key gains a leg here on the same
# commit, and one whose key stops doing anything fails loudly instead of
# quietly going unswept.
#
# Needs `tmux` and the pinned `nvim`. No agent: the panel is opened against
# the stub agent the conformance leg already builds, since what is under
# test is the paint, never the conversation. No network either, unless a
# borrowed config's own plugins reach for one.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/acceptance/artifacts.sh
. "$SCRIPT_DIR/artifacts.sh"
VIEW_BIN=${VIEW_BIN:-$TARGET_ROOT/release/view}
STUB_BIN=${STUB_BIN:-$TARGET_ROOT/release/view-ai-stub-agent}
FIXTURE=$SCRIPT_DIR/fixtures/themed
# The XDG_CONFIG_HOME the driven sessions read, and the XDG_DATA_HOME whose
# installed plugins they borrow. Both are directories rather than files:
# nothing here reads a config's text.
SWEEP_CONFIG=${VIEW_SWEEP_CONFIG:-$FIXTURE}
SWEEP_DATA=${VIEW_SWEEP_DATA:-}
MAPPINGS_RS=$REPO_ROOT/crates/view-core/src/native/mappings.rs
PANEL_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/mod.rs
PERMISSION_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/permission.rs
PICKER_RS=$REPO_ROOT/crates/view-core/src/native/picker.rs
PALETTE_RS=$REPO_ROOT/crates/view-core/src/native/palette.rs
SURFACES_RS=$REPO_ROOT/crates/view-core/src/update/surfaces.rs
OVERLAY_RS=$REPO_ROOT/crates/view-surface/src/overlay.rs
NVIM_API_RS=$REPO_ROOT/crates/view-engine/src/nvim_api.rs

# The pane most legs read. The width is what the agent panel's own title
# needs whole: the panel takes a fixed share of the terminal, and its
# focused title is the longest label any overlay here sets into a border.
COLS=140
ROWS=44
# The pane `leg_narrow_title` reads instead: the agent panel's share of it
# is a shorter top edge than that same title, so the panel is named there by
# what survives the cut. A laptop-width terminal, not a contrived one.
NARROW_COLS=100
# How often the screen is read.
POLL=0.25
# How long any single observation is given. Generous: nothing here is a
# latency measurement, and a loaded host must not flake.
WAIT_SECS=25
# The window a driven entry point has to change the screen in, and the one a
# keystroke burst has to appear in. A liveness bound, not a paint budget --
# the numbers in the spec's own latency table are measured by `view-bench`
# against a quiet host, never through tmux -- and it is orders of magnitude
# looser than any of them, because what it exists to catch is a gesture that
# does nothing at all.
REACTION_SECS=5
# The mark the transcript opens a prompt the user sent with, and the frames
# it wears instead while that prompt's turn is still in flight. Read off the
# panel's own source (`mark_str`, `spinner_alternation`), never spelled
# here.
#
# What a leg asserts a prompt with, rather than the text alone: the composer
# holds what was typed whether the panel took it as a prompt or refused it,
# so text on screen says nothing about which happened. The mark does.
USER_MARK=$(mark_str USER_MARK) || exit 1
SPINNER_ALTERNATION=$(spinner_alternation) || exit 1
# The two together, as one alternation: a prompt the panel has taken opens
# with the settled mark or with whichever frame the screen was read on. The
# space the settled mark carries is dropped here, since the frames beside it
# are the glyphs alone.
TAKEN_MARKS="${USER_MARK% }|$SPINNER_ALTERNATION"

SESSIONS=()
ROOTS=()
SESSION=""
ROOT=""
# The stub agent's hold-and-release gate (crates/view-ai/tests/fixtures/stub_agent.rs):
# named here so `cleanup` can remove it whichever leg created it.
RESUME_FILE=""
CURRENT_LEG=startup
DUMP_DIR=$(dump_dir view-visual-sweep)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-work-XXXXXX")
CAP=$WORK/pane.esc
CELLS=$WORK/pane.cells
SCREEN=$WORK/pane.txt
BEFORE=$WORK/before.txt
# Whatever the palette probe wrote to stderr, kept for the failure that
# names an unanswered group.
PROBE_ERR=$WORK/probe.err
: >"$SCREEN"
: >"$BEFORE"

cleanup() {
    local code=$?
    local session root
    reap_views || code=1
    for session in ${SESSIONS[@]+"${SESSIONS[@]}"}; do
        tmux kill-session -t "$session" 2>/dev/null || true
    done
    for root in ${ROOTS[@]+"${ROOTS[@]}"}; do
        [ -n "$root" ] && rm -rf "$root"
    done
    rm -rf "$WORK"
    rm -f "$RESUME_FILE"
    if [ "$code" -eq 0 ] || [ -z "$(ls -A "$DUMP_DIR" 2>/dev/null)" ]; then
        rm -rf "$DUMP_DIR"
    else
        printf '      pane dumps kept in %s\n' "$DUMP_DIR" >&2
    fi
    exit "$code"
}
# The signals are named one at a time rather than shared with the EXIT trap,
# and each names the status a shell killed by it reports (128 + signal).
# `trap cleanup EXIT INT TERM` reads as though it covered them, and it does
# run the cleanup -- but the handler enters with `$?` from whatever the leg
# was doing, so the `exit "$code"` at its end reported 0 and an interrupted
# run read as a passing one in the log. HUP was not on the list at all,
# which is the signal an ssh session dropping under a running leg delivers.
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# One screen cell per line: row, column, background, reverse flag,
# underline flag, glyph, tab separated.
#
# Written as a normalized table rather than asserted on in place because the
# capture is a stream of style runs, not a grid: which column a cell sits in
# is only knowable by replaying every escape sequence before it, and an
# assertion that greps the raw bytes cannot say whether the default
# background it found was inside a box or in the buffer beside it. Style
# state deliberately carries across input lines -- tmux emits a run's
# sequence only when it changes, so the first cells of a row inherit the
# last style of the row above.
#
# Byte oriented (every caller runs it under `LC_ALL=C`): a box-drawing glyph
# is three bytes, and an awk that counted those as three columns would
# misplace every cell to the right of the first border on the row. The
# continuation bytes are folded back into their leading byte here, which is
# correct in any awk rather than only in one with multibyte support.
CELL_AWK='
function apply(params,   a, n, k, v) {
    if (params == "") params = "0"
    n = split(params, a, ";")
    for (k = 1; k <= n; k++) {
        v = a[k] + 0
        if (v == 0) { bg = "-"; rev = 0; und = 0 }
        else if (v == 7) rev = 1
        else if (v == 27) rev = 0
        else if (a[k] == "4:0") und = 0
        else if (v == 4) und = 1
        else if (v == 24) und = 0
        else if (v == 49) bg = "-"
        else if (v == 48) {
            if (a[k + 1] + 0 == 2) { bg = a[k + 2] ";" a[k + 3] ";" a[k + 4]; k += 4 }
            else if (a[k + 1] + 0 == 5) { bg = "p" a[k + 2]; k += 2 }
        }
        # a foreground (or underline) color is skipped whole. Its channels
        # are ordinary numbers and collide with every code above -- a blue
        # of 7 reads as reverse video, of 49 as the terminal default
        # background, of 41 as a background nothing here paints -- and
        # since tmux emits a run only when it changes, one such collision
        # corrupts every following cell until the next real background,
        # across row boundaries included
        else if (v == 38 || v == 58) {
            if (a[k + 1] + 0 == 2) k += 4
            else if (a[k + 1] + 0 == 5) k += 2
        }
        else if (v >= 40 && v <= 47) bg = "b" v
        else if (v >= 100 && v <= 107) bg = "b" v
    }
}
BEGIN {
    for (i = 0; i < 256; i++) byte[sprintf("%c", i)] = i
    esc = sprintf("%c", 27)
    bg = "-"
    rev = 0
    und = 0
}
{
    row = NR - 1
    col = 0
    i = 1
    n = length($0)
    while (i <= n) {
        if (substr($0, i, 1) == esc) {
            j = i + 1
            if (substr($0, j, 1) == "[") {
                j++
                while (j <= n && index("0123456789;:<=>?", substr($0, j, 1)) > 0) j++
                if (substr($0, j, 1) == "m") apply(substr($0, i + 2, j - i - 2))
            }
            i = j + 1
            continue
        }
        glyph = substr($0, i, 1)
        j = i + 1
        while (j <= n && byte[substr($0, j, 1)] >= 128 && byte[substr($0, j, 1)] < 192) {
            glyph = glyph substr($0, j, 1)
            j++
        }
        printf "%d\t%d\t%s\t%d\t%d\t%s\n", row, col, bg, rev, und, glyph
        col++
        i = j
    }
}'

# One glyph of one `BorderSet`, read out of the charset that carries it:
# `border_glyph ROUNDED top_left`. Everything here that looks for a frame is
# built from these rather than from its own copy of the literals -- the day
# the charset changed, three readers that had spelled the corners themselves
# went blind at once and every one of them failed by claiming the box was
# never drawn.
border_glyph() {
    awk -v set="$1" -v field="$2" '
        $0 ~ ("pub const " set) { inside = 1 }
        # a `const` name resolves through its own declaration, so a field
        # written as one (`horizontal: LINE_H`) reads the same as a literal
        /^const [A-Z_]+: char = / {
            q = sprintf("%c", 39)
            name = substr($0, 7, index($0, ":") - 7)
            rest = substr($0, index($0, "= " q) + 3)
            alias[name] = substr(rest, 1, index(rest, q) - 1)
            next
        }
        inside && $0 ~ ("^ +" field ": ") {
            # cut on the quotes rather than on a character count: a glyph is
            # up to three bytes and awk counts bytes in this locale
            q = sprintf("%c", 39)
            if (index($0, q) == 0) {
                split($0, parts, /[:, ]+/)
                print alias[parts[3]]
                exit
            }
            s = substr($0, index($0, field ": " q) + length(field) + 3)
            print substr(s, 1, index(s, q) - 1)
            exit
        }
    ' "$OVERLAY_RS"
}

# The frame glyphs every reader below pairs rows on. The sweep drives a UTF-8
# tmux pane whose box-glyph probe answers yes, so `ROUNDED` is the set on
# screen and the ASCII one cannot be: its corners are `+` and its edge `|`,
# both of which occur in ordinary buffer text, and admitting them would read
# a line of source as a box.
BOX_TL=$(border_glyph ROUNDED top_left)
BOX_TR=$(border_glyph ROUNDED top_right)
BOX_BL=$(border_glyph ROUNDED bottom_left)
BOX_BR=$(border_glyph ROUNDED bottom_right)
BOX_V=$(border_glyph ROUNDED vertical)
# A corner that came back empty, or one a terminal draws in ordinary text,
# would make every box reader below answer "no box on screen" for a screen
# full of them -- silently, since that is also the honest answer when the box
# really is absent.
for glyph in "$BOX_TL" "$BOX_TR" "$BOX_BL" "$BOX_BR" "$BOX_V"; do
    case "$glyph" in
    '' | ' ' | '-' | '|' | '+' | '=')
        printf 'FAIL: a ROUNDED border glyph in %s reads as %s, which ordinary text is full of, so no reader here could tell a box from a buffer\n' \
            "$OVERLAY_RS" "${glyph:-nothing this can read}" >&2
        exit 1
        ;;
    esac
done

# Where the framed boxes on screen are, shared by everything that has a
# question about their insides.
#
# A row is scanned for border glyphs and they are paired left to right, so
# two boxes side by side (a sidebar and a centered overlay, a panel and a
# toast) are two independent spans rather than one span swallowing the
# buffer between them. An odd border glyph out -- a box clipped by the
# pane's edge -- ends the row's pairing rather than reaching to its end.
# The glyphs come in as `-v`, from `border_glyph` above, so the charset is
# read out of `BorderSet` and never re-spelled here.
BOX_AWK='
function is_edge(g) {
    return (g == BOX_V || g == BOX_TL || g == BOX_TR || g == BOX_BL || g == BOX_BR)
}
# a box whose side is covered by a box drawn over it leaves the row with a
# corner where its other side should be, and pairing across that would
# sweep the buffer between two boxes as if it were an interior.
# Under-checking such a row is the safe way to be wrong; reporting the
# buffer as a bleed is not.
function clipped(l, r) {
    return (l == BOX_TR || l == BOX_BR || r == BOX_TL || r == BOX_BL)
}
function edges_of(row, edge,   c, n) {
    n = 0
    for (c = 0; c <= widest[row]; c++)
        if (is_edge(glyph[row "," c])) edge[++n] = c
    return n
}
BEGIN { FS = "\t" }
{
    key = $1 "," $2
    back[key] = $3
    reverse[key] = $4
    under[key] = $5
    glyph[key] = $6
    if ($2 > widest[$1]) widest[$1] = $2
    rows[$1] = 1
}'

# Every framed box on screen, checked cell by cell from its left border to
# its right one.
CHROME_AWK='
function note(row, col, what) {
    findings++
    if (findings <= 6) printf "      row %d col %d carries %s\n", row, col, what
}
{
    if ($4 == 1) {
        reversed++
        if (reversed <= 6) printf "      row %d col %d is reverse video\n", $1, $2
    }
}
END {
    for (r in rows) {
        edges = edges_of(r, edge)
        for (k = 1; k + 1 <= edges; k += 2) {
            if (clipped(glyph[r "," edge[k]], glyph[r "," edge[k + 1]])) continue
            for (c = edge[k]; c <= edge[k + 1]; c++) {
                key = r "," c
                cells++
                if (back[key] == "-") note(r, c, "the terminal default background")
                else if (back[key] in beneath) note(r, c, "the background of the layer beneath (" back[key] ")")
                else {
                    if (back[key] == float_bg) themed++
                    # the only group the fixture underlines is a buffer-layer
                    # one, so an underlined cell inside a box is an attribute
                    # the overlay inherited rather than set
                    if (under[key] == 1) note(r, c, "the underline of the layer beneath")
                }
            }
        }
    }
    if (cells == 0) {
        print "      no framed overlay on screen at all, so nothing was checked"
        findings++
    } else if (themed == 0) {
        print "      no overlay cell carries the colorscheme float background, so the box is not themed"
        findings++
    }
    exit (findings > 0 || reversed > 0)
}'

# Where an ASCII string is painted, and where the vertical borders left of
# it on that same row are: "row col leftmost nearest", or nothing at all when
# the string is not on screen. `nearest` is the last border before the text
# and `leftmost` the first on the row, so the two differ only when something
# other than the box the text sits in has an edge further left -- which is
# the whole of "this box is drawn over that one" and not merely beside it.
# Both are -1 when the row has no border left of the text at all.
#
# Column-indexed rather than derived from the plain-text screen, because a
# byte offset into a row is not a column -- every box-drawing glyph on it is
# three bytes -- and the whole question this answers is which columns the
# string occupies relative to a box's edge.
#
# The lowest matching row wins rather than the first one awk happens to walk
# to: `for (r in rows)` visits an array in hash order, so a string that also
# appears in the buffer, a transcript or the message history would otherwise
# be reported from whichever copy the walk reached first, and the border
# columns beside it answered for the wrong row.
SPAN_AWK='
function match_col(r, text,   n, c, k, ok) {
    n = length(text)
    for (c = 0; c + n - 1 <= widest[r]; c++) {
        ok = 1
        for (k = 1; k <= n; k++)
            if (glyph[r "," (c + k - 1)] != substr(text, k, 1)) { ok = 0; break }
        if (ok) return c
    }
    return -1
}
BEGIN { FS = "\t" }
{
    glyph[$1 "," $2] = $6
    if ($2 > widest[$1]) widest[$1] = $2
    rows[$1] = 1
}
END {
    hit = -1
    for (r in rows) {
        c = match_col(r, text)
        if (c >= 0 && (hit < 0 || r + 0 < hit)) { hit = r + 0; col = c }
    }
    if (hit < 0) exit
    leftmost = -1
    nearest = -1
    for (b = 0; b < col; b++)
        if (glyph[hit "," b] == "\342\224\202") {
            if (leftmost < 0) leftmost = b
            nearest = b
        }
    printf "%d %d %d %d\n", hit, col, leftmost, nearest
}'

# The text inside each framed box, one span per line.
#
# What a surface paints into its own frame -- its title on the top edge
# included, which is where every native overlay writes the one word that
# names it. A match against the whole screen cannot tell that word from the
# same word in the buffer, the statusline or a toast, and "the key opened
# the right feature" is exactly the claim that distinction carries.
BOX_TEXT_AWK='
END {
    for (r in rows) {
        edges = edges_of(r, edge)
        for (k = 1; k + 1 <= edges; k += 2) {
            if (clipped(glyph[r "," edge[k]], glyph[r "," edge[k + 1]])) continue
            line = ""
            for (c = edge[k]; c <= edge[k + 1]; c++) line = line glyph[r "," c]
            print line
        }
    }
}'

# The current screen, as a cell table and as the plain text derived from it.
# One capture serves both, so a wait that matched some text and an assertion
# about the colors under it are reading the same frame rather than two
# frames a poll apart.
capture() {
    tmux capture-pane -t "$SESSION" -p -e >"$CAP" 2>/dev/null || : >"$CAP"
    LC_ALL=C awk "$CELL_AWK" "$CAP" >"$CELLS"
    LC_ALL=C awk -F'\t' '
        { if (NR > 1 && $1 != prev) printf "\n"; printf "%s", $6; prev = $1 }
        END { if (NR > 0) printf "\n" }' "$CELLS" >"$SCREEN"
}

fail() {
    local dump="$DUMP_DIR/$CURRENT_LEG"
    cp "$CAP" "$dump.esc" 2>/dev/null || true
    cp "$SCREEN" "$dump.pane" 2>/dev/null || true
    cp "$CELLS" "$dump.cells" 2>/dev/null || true
    printf 'FAIL [%s]: %s\n' "$CURRENT_LEG" "$1" >&2
    printf '      pane dump: %s.pane (escapes: %s.esc, cells: %s.cells)\n' \
        "$dump" "$dump" "$dump" >&2
    # the leg's own roots are wiped by `cleanup`, and on a CI runner the
    # whole filesystem goes with the job, so anything a reader needs has to
    # be inside the one directory a failing run keeps
    if [ -n "$ROOT" ] && [ -f "$ROOT/view.log" ]; then
        cp "$ROOT/view.log" "$dump.log" 2>/dev/null || true
        printf '      view log: %s.log\n' "$dump" >&2
    fi
    if [ -n "${STATE_HOME:-}" ] && [ -d "$STATE_HOME/view" ]; then
        cp -R "$STATE_HOME/view" "$dump.state" 2>/dev/null || true
        printf '      state: %s.state\n' "$dump" >&2
    fi
    return 1
}

command_line() {
    send_text "$1"
    send_key Enter
}

# What holds `key` in normal mode in the running session: the description
# the live mapping carries, `-nodesc-` for a mapping registered without
# one, or `-none-` when nothing maps the key at all.
#
# A user's own mapping outranks a default, correctly, so a config that
# binds a key view also binds owns it -- and a leg pressing that key would
# drive the config's feature rather than the surface under test. view
# registers its defaults with a description of its own (`view: <feature>
# <verb>`, `nvim_api.rs`), which is what lets the caller tell the two apart
# from the live session instead of from an assumption about the config.
# Long brackets around both arguments so a leader nvim spells with a
# backslash reaches Lua as the character it is.
mapping_desc() {
    local key="$1" answer=$WORK/mapping-desc.txt lua start el read
    rm -f "$answer" "$answer.part"
    # written aside and renamed into place: `writefile` creates the file and
    # then fills it, so a reader that waits on existence alone can take a
    # zero-byte answer -- which reads as a description of `''`, and an empty
    # description is what the caller treats as "the config owns this key".
    # A rename is the one step nothing can observe half of.
    lua=":lua local m = vim.fn.maparg([==[$key]==], 'n', false, true)"
    lua="$lua local d = next(m) == nil and '-none-' or m.desc"
    lua="$lua if d == nil or d == '' then d = '-nodesc-' end"
    lua="$lua vim.fn.writefile({ d }, [==[$answer.part]==])"
    lua="$lua os.rename([==[$answer.part]==], [==[$answer]==])"
    command_line "$lua"
    start=$(now)
    while [ ! -f "$answer" ]; do
        el=$(elapsed "$start" "$(now)")
        under "$el" "$REACTION_SECS" || {
            fail "the session never answered which mapping holds $key"
            return 1
        }
        sleep "$POLL"
    done
    read=$(head -n 1 "$answer")
    [ -n "$read" ] || {
        fail "the session answered nothing at all for the mapping on $key, which must not be read as a config own key"
        return 1
    }
    printf '%s' "$read"
}

# Waits for `text` to be on screen, answering how long that took.
wait_for() {
    local text="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        capture
        if grep -qF -- "$text" "$SCREEN"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        if ! tmux has-session -t "$SESSION" 2>/dev/null; then
            fail "the view session exited while waiting for $what"
            return 1
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what never appeared: no '$text' on screen after ${budget}s"
            return 1
        fi
        sleep "$POLL"
    done
}

# Waits for the screen to differ from the one [`mark`] recorded. The whole
# of the silent-no-op assertion: a gesture that reaches nothing paints
# nothing, and no wait on a particular string can say that about an entry
# point whose output this script does not enumerate.
mark() { cp "$SCREEN" "$BEFORE"; }
wait_change() {
    local budget="$1" what="$2" start el
    start=$(now)
    while :; do
        capture
        # the exit check comes before the difference check, not after it: a
        # session that died captures an empty screen, which differs from
        # every mark, and "the screen changed" would send a reader after a
        # paint defect that is really a crash
        if ! tmux has-session -t "$SESSION" 2>/dev/null; then
            fail "the view session exited while waiting for $what"
            return 1
        fi
        if ! cmp -s "$SCREEN" "$BEFORE"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what changed nothing on screen within ${budget}s, which is a gesture that did not reach its feature"
            return 1
        fi
        sleep "$POLL"
    done
}

# Waits until nothing on screen is framed any more.
wait_no_box() {
    local budget="$1" what="$2" start el
    start=$(now)
    while :; do
        capture
        # ahead of the frame check, not after it: a session that died
        # captures an empty screen, which holds no frame character, and
        # "nothing is framed any more" would report a crash as the very
        # dismissal under test having worked
        if ! tmux has-session -t "$SESSION" 2>/dev/null; then
            fail "the view session exited while waiting for $what"
            return 1
        fi
        grep -qF "$BOX_TL" "$SCREEN" || return 0
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what is still framed on screen after ${budget}s"
            return 1
        fi
        sleep "$POLL"
    done
}

# The text of every framed box in the last capture.
box_text() { LC_ALL=C awk -v BOX_TL="$BOX_TL" -v BOX_TR="$BOX_TR" -v BOX_BL="$BOX_BL" -v BOX_BR="$BOX_BR" -v BOX_V="$BOX_V" "$BOX_AWK$BOX_TEXT_AWK" "$CELLS"; }

# "row col leftmost nearest" for `text` in the last capture; empty when it is
# not on screen at all.
text_span() { LC_ALL=C awk -v text="$1" "$SPAN_AWK" "$CELLS"; }

# The background the first cell of `text` is painted on, in the buffer's own
# region of the screen -- nothing at all when it is not on screen, or when
# some overlay's edge stands to its left.
#
# The region matters as much as the color: a decoration in the file is left
# of every frame on its row, where a panel row is right of one. Both halves
# are the claim "the proposal is drawn where the code is".
buffer_bg() {
    local span row col leftmost
    span=$(text_span "$1")
    [ -n "$span" ] || return 0
    read -r row col leftmost _ <<<"$span"
    [ "$leftmost" -lt 0 ] || return 0
    LC_ALL=C awk -F'\t' -v r="$row" -v c="$col" '$1 == r && $2 == c { print $3; exit }' "$CELLS"
}

# Fails unless `text` is painted in the buffer region on the colorscheme's
# own `want` background.
assert_buffer_bg() {
    local text="$1" want="$2" what="$3" got
    settle
    got=$(buffer_bg "$text")
    if [ "$got" != "$want" ]; then
        fail "$what reads '${got:-nothing in the buffer region}' where the colorscheme paints $want"
        return 1
    fi
    pass "$what is painted on $want, left of every frame on its row"
}

# Waits for `text` to be painted inside a framed overlay -- its title bar
# included -- rather than anywhere on screen.
wait_in_box() {
    local text="$1" budget="$2" what="$3" start el
    start=$(now)
    while :; do
        capture
        if holds "$text" "$(box_text)"; then
            elapsed "$start" "$(now)"
            return 0
        fi
        if ! tmux has-session -t "$SESSION" 2>/dev/null; then
            fail "the view session exited while waiting for $what"
            return 1
        fi
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what never appeared: no '$text' inside a framed overlay after ${budget}s"
            return 1
        fi
        sleep "$POLL"
    done
}

# Holds until two consecutive captures agree.
#
# crossterm's frame write is not atomic with respect to tmux's reads, so a
# capture can land mid-frame: a half-painted overlay whose remaining rows
# still show the layer beneath, which is indistinguishable from the defect
# this leg exists to catch. A screen that never settles is asserted on
# anyway -- a false pass is not what this guards against, and a sweep that
# gave up here would be reporting a paint budget it does not measure.
settle() {
    local prev=$WORK/settle.cells
    for _ in 1 2 3 4 5 6 7 8; do
        cp "$CELLS" "$prev"
        sleep "$POLL"
        capture
        cmp -s "$CELLS" "$prev" && return 0
    done
    return 0
}

# The pane's own cursor: the row, the column, and whether the terminal is
# showing it -- "row col flag".
#
# Read from tmux rather than from a capture because it is the one thing a
# capture cannot answer: the hardware caret is terminal state, not a cell,
# and the defect this exists for (a composer with no insertion point, the
# caret left behind in nvim's grid while every key landed in the panel) is
# invisible in every screen dump of it.
caret_cell() { tmux display -p -t "$SESSION" '#{cursor_y} #{cursor_x} #{cursor_flag}'; }

# Fails unless the pane's caret is visible and standing one cell past
# `text` -- the insertion point, where the next character lands.
assert_caret_after() {
    local text="$1" what="$2" span row col got want
    settle
    span=$(text_span "$text")
    [ -n "$span" ] || {
        fail "$what: '$text' is not on screen at all, so there is no insertion point to check"
        return 1
    }
    read -r row col _ _ <<<"$span"
    want="$row $((col + ${#text})) 1"
    got=$(caret_cell)
    [ "$got" = "$want" ] || {
        fail "$what: the terminal's caret reads '$got' (row col visible), not '$want' -- one cell past '$text'"
        return 1
    }
    pass "$what: the caret stands one cell past '$text' (row $row col $((col + ${#text})))"
}

# Fails unless the pane's caret is visible and left of the framed box on its
# own row, which is where nvim's grid is: the buffer keeps the caret whenever
# the panel is not the thing keys reach.
#
# A row with no box edge on it cannot answer the question -- "outside the box"
# and "the box is not here" read the same -- so that is a failure too, and the
# caller is pointed at a row the panel actually spans.
assert_caret_outside_a_box() {
    local what="$1" row col flag edge
    settle
    read -r row col flag <<<"$(caret_cell)"
    [ "$flag" = 1 ] || {
        fail "$what: the caret is hidden (flag '$flag'), so the user is shown no insertion point at all"
        return 1
    }
    edge=$(LC_ALL=C awk -F'\t' -v r="$row" \
        '$1 == r && $6 == "\342\224\202" { print $2; exit }' "$CELLS")
    [ -n "$edge" ] || {
        fail "$what: no framed box edge on row $row, so this row proves nothing about which surface holds the caret"
        return 1
    }
    [ "$col" -lt "$edge" ] || {
        fail "$what: the caret is at row $row col $col, right of the box edge at col $edge, so it is parked in an overlay that does not have the keys"
        return 1
    }
    pass "$what: the caret is at row $row col $col, left of the framed box on its row (col $edge)"
}

# The column the agent panel's left frame edge stands in.
#
# The panel is pinned to one edge of the terminal, so that column is its
# width read off the screen -- the only reading of a resize there is, since
# the width itself is state no capture holds. Leftmost across the whole
# capture, because the legs that call it have exactly one framed surface up.
panel_edge() {
    settle
    LC_ALL=C awk -F'\t' '$6 == "\342\224\202" { if (min == "" || $2 < min) min = $2 }
        END { if (min != "") print min }' "$CELLS"
}

# The glyph the pane's caret is standing on, or nothing when the cell it
# names holds none.
#
# Read by joining tmux's own cursor to the capture rather than from either
# alone: which key a caret is offering to press is a claim about both, and
# the two are separate readings of the same frame.
caret_glyph() {
    local row col
    read -r row col _ <<<"$(caret_cell)"
    LC_ALL=C awk -F'\t' -v r="$row" -v c="$col" '$1 == r && $2 == c { print $6; exit }' "$CELLS"
}

# How many screen rows the transcript entry opening with `head` spans:
# its own row and every filled row under it, up to the first blank one.
# Nothing at all when `head` is off screen.
#
# Counted by rows rather than by finding a marker on the entry's last one,
# because a re-wrap is exactly what puts a marker across a row boundary --
# the tail of a narrowed entry reads as `REF` / `LOWTAIL` and no span
# search finds it. What a row holds is asked of the cells between the
# panel's own two edges, so the buffer beside it cannot fill a row here.
entry_rows() {
    local head="$1" span head_row
    settle
    span=$(text_span "$head")
    [ -n "$span" ] || return 0
    read -r head_row _ _ _ <<<"$span"
    LC_ALL=C awk -F'\t' -v head="$head_row" '
        {
            glyph[$1 "," $2] = $6
            if ($6 == "\342\224\202") {
                if (!($1 in lo) || $2 < lo[$1]) lo[$1] = $2
                if (!($1 in hi) || $2 > hi[$1]) hi[$1] = $2
            }
            if ($1 > last) last = $1
        }
        END {
            for (r = head; r <= last; r++) {
                if (!(r in lo) || !(r in hi) || hi[r] <= lo[r] + 1) break
                filled = 0
                for (c = lo[r] + 1; c < hi[r]; c++) {
                    g = glyph[r "," c]
                    if (g != "" && g != " ") { filled = 1; break }
                }
                if (!filled) break
                rows++
            }
            print rows + 0
        }' "$CELLS"
}

# Everything framed on screen, in row order, with the frame, the row breaks
# and the padding taken out.
#
# A wrapped entry is the one place a marker cannot be looked for as it was
# typed: the wrap puts a row boundary through the middle of it, and after a
# re-wrap the boundary is somewhere else -- a narrowed `REFLOWTAIL` reads as
# `REF` on one row and `LOWTAIL` on the next. Joining is what lets a leg ask
# whether the text survived without also asserting where it broke. Built
# from the cells rather than from [`box_text`], whose rows come out in awk's
# own order for an associative array: unordered rows joined are a different
# string every run.
box_text_joined() {
    settle
    LC_ALL=C awk -F'\t' '
        {
            glyph[$1 "," $2] = $6
            if ($6 == "\342\224\202") {
                if (!($1 in lo) || $2 < lo[$1]) lo[$1] = $2
                if (!($1 in hi) || $2 > hi[$1]) hi[$1] = $2
            }
            if ($1 > last) last = $1
        }
        END {
            for (r = 0; r <= last; r++) {
                if (!(r in lo) || !(r in hi)) continue
                for (c = lo[r] + 1; c < hi[r]; c++) {
                    g = glyph[r "," c]
                    if (g != "" && g != " ") printf "%s", g
                }
            }
            printf "\n"
        }' "$CELLS"
}

assert_chrome() {
    local what="$1" findings
    settle
    findings=$(LC_ALL=C awk -v float_bg="$FLOAT_BG" \
        -v beneath_normal="$NORMAL_BG" -v beneath_cursor="$CURSORLINE_BG" \
        -v BOX_TL="$BOX_TL" -v BOX_TR="$BOX_TR" -v BOX_BL="$BOX_BL" -v BOX_BR="$BOX_BR" -v BOX_V="$BOX_V" \
        'BEGIN { beneath[beneath_normal] = 1; beneath[beneath_cursor] = 1 }'"$BOX_AWK$CHROME_AWK" \
        "$CELLS") && return 0
    printf '%s\n' "$findings" >&2
    fail "$what is not painted opaquely in the colorscheme's own colors"
    return 1
}

# One session, against the themed fixture and the stub agent. The config
# directory is shared across legs on purpose: the theme cache is keyed by
# the resolved config path, so a per-leg copy of the fixture would key a
# different cache file and every leg would run its first frames unthemed.
# The state directory is shared for the same reason -- it is where that
# cache lives, already warmed. Everything a leg writes that another leg must
# not see (the AI trust store, the working tree it opens) is its own.
start_session() {
    local tag="$1" seed="$2"
    SESSION="view-visual-$$-$tag"
    ROOT=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-$tag-XXXXXX")
    ROOTS+=("$ROOT")
    mkdir -p "$ROOT/xdg_cache_home"
    # long enough that the cursor can sit in the middle of the window: a
    # centered overlay has to be drawn over the cursor row's full-width
    # highlight for the interior check to have anything to catch, and a
    # one-line buffer keeps that row at the very top where nothing lands
    {
        printf '%s\n' "$seed"
        awk -v n="$ROWS" 'BEGIN { for (i = 2; i <= n; i++) printf "visual sweep filler line %d\n", i }'
    } >"$ROOT/scratch.txt"

    tmux kill-session -t "$SESSION" 2>/dev/null || true
    SESSIONS+=("$SESSION")
    # started in $ROOT, which is both the project root view offers to trust
    # and a directory no trust store here has ever heard of
    tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" -c "$ROOT" \
        "env XDG_CONFIG_HOME=$SWEEP_CONFIG \
             XDG_DATA_HOME=$DATA_HOME \
             XDG_STATE_HOME=$STATE_HOME \
             XDG_CACHE_HOME=$ROOT/xdg_cache_home \
             VIEW_LOG=$ROOT/view.log \
             TERM=xterm-256color COLORTERM=truecolor \
             $LAUNCHER $ROOT/scratch.txt"

    wait_for "$seed" "$WAIT_SECS" "the seeded buffer" >/dev/null || return 1
    watch_view "$SESSION" || return 1
    # One window, no floats: a config with a file explorer or a startup
    # notice of its own opens both, and every assertion here that reads
    # "the framed box on this row" or "left of every frame" would answer
    # for a plugin's window rather than for the surface under test. `only`
    # and a close of every relative window say that in nvim's own terms, so
    # no plugin is named and none has to be.
    command_line ':silent! only'
    command_line ':lua for _, w in ipairs(vim.api.nvim_list_wins()) do if vim.api.nvim_win_get_config(w).relative ~= "" then pcall(vim.api.nvim_win_close, w, true) end end'
    # anchored on the ruler rather than on any buffer text, which was
    # already on screen before the motion and so would report the cursor
    # moved whether it had or not
    local middle=$((ROWS / 2))
    send_text "${middle}G"
    wait_for "$middle,1" "$WAIT_SECS" "the cursor on the middle line" >/dev/null || return 1
}

# Puts the screen back to a bare buffer. `ai` needs naming because Escape
# only leaves the panel's composer; the panel itself stays framed, and every
# following assertion about "the box that just opened" would otherwise be
# reading it.
dismiss() {
    local feature="${1:-}"
    send_key Escape
    if [ "$feature" = ai ]; then
        command_line ':View ai close'
    fi
    wait_no_box 15 "the overlay" >/dev/null || return 1
}

for tool in tmux awk grep cmp; do
    command -v "$tool" >/dev/null 2>&1 || {
        printf 'FAIL: %s is not on PATH; this acceptance drives a real terminal\n' "$tool" >&2
        exit 1
    }
done
# The cell reader against a capture whose answer is known: two truecolor
# foregrounds over one unchanged background, the second spelled out of
# channels that are also SGR codes (7 = reverse video, 49 = the terminal
# default background, 41 = a red background). Every cell here is opaque,
# upright and painted on 33;34;44; a reader that let a foreground channel
# through would report the last two as reverse video on the default
# background, and would keep reporting it for every cell after them,
# because tmux emits a background only when it changes.
esc=$(printf '\033')
selftest=$(printf '%s[48;2;33;34;44m%s[38;2;248;248;242mAA%s[38;2;7;49;41mBB\n' \
    "$esc" "$esc" "$esc" | LC_ALL=C awk "$CELL_AWK" |
    awk -F'\t' '$3 != "33;34;44" || $4 != 0 || $5 != 0 { print }')
if [ -n "$selftest" ]; then
    printf 'FAIL: the cell reader mis-reads a foreground color as a background or an attribute\n' >&2
    printf '%s\n' "$selftest" >&2
    exit 1
fi

ensure_artifact "$VIEW_BIN" "$TARGET_ROOT/release/view" \
    cargo build --release -p view || exit 1
ensure_artifact "$STUB_BIN" "$TARGET_ROOT/release/view-ai-stub-agent" \
    cargo build --release -p view-ai --features test-support --bin view-ai-stub-agent || exit 1
[ -d "$FIXTURE" ] || {
    printf 'FAIL: the themed fixture is missing at %s\n' "$FIXTURE" >&2
    exit 1
}
[ -d "$SWEEP_CONFIG/nvim" ] || {
    printf 'FAIL: %s is not an XDG_CONFIG_HOME (no nvim/ under it), so there is no colorscheme for this run to be driven against\n' \
        "$SWEEP_CONFIG" >&2
    exit 1
}
# absolute from here on: both of these are handed to nvim as XDG homes, and
# a relative one resolves against whatever directory each process happens
# to have -- the sessions are started from tmux with a root of their own
SWEEP_CONFIG=$(cd -- "$SWEEP_CONFIG" && pwd)
[ -z "$SWEEP_DATA" ] || SWEEP_DATA=$(cd -- "$SWEEP_DATA" && pwd)

# The XDG environment every session below runs under, and the probe with
# it: one home per kind, built once, so a leg's cache hit and the probe's
# palette are readings of the same configured editor. `RUN_SUPPORT` is not
# one of those homes -- the XDG_CONFIG_HOME is the borrowed `SWEEP_CONFIG`
# -- it is where this run keeps the files it composes for itself: the
# `view.toml` it drives with, the engine prelude, the launcher.
RUN_SUPPORT=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-support-XXXXXX")
DATA_HOME=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-data-XXXXXX")
STATE_HOME=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-state-XXXXXX")
ROOTS+=("$RUN_SUPPORT" "$DATA_HOME" "$STATE_HOME")
# The plugin set, linked in: `nvim/lazy` is where a plugin manager finds
# what is already installed, and it is the only path under a borrowed data
# home this run can reach. Everything a plugin writes elsewhere -- a tool
# installer's downloads, an auto-saved session, a project history -- has
# nowhere to land but the scratch homes above.
#
# The link is to the directory, so it is writable, and what closes that is
# the prelude below rather than the link: a plugin manager asked to install
# a spec it cannot find would clone straight through it, and its own clean
# step deletes the directory first. Stubbing the module that installs is
# what makes the borrow read-only -- a link nothing can be written through
# because nothing that writes exists in the session. What still reaches the
# borrowed home is a plugin writing inside its own directory at runtime (a
# compiled treesitter parser, a plugin-local cache), which is named here
# rather than claimed away.
#
# `cleanup` wipes these roots with `rm -rf`, which unlinks a symlink instead
# of walking into it; there is no glob and no `find -delete` anywhere near
# them, and that is what keeps a scratch teardown off the user's plugins.
if [ -n "$SWEEP_DATA" ]; then
    [ -d "$SWEEP_DATA/nvim/lazy" ] || {
        printf 'FAIL: %s holds no nvim/lazy, so there is no installed plugin set to borrow\n' \
            "$SWEEP_DATA" >&2
        exit 1
    }
    mkdir -p "$DATA_HOME/nvim"
    ln -s "$SWEEP_DATA/nvim/lazy" "$DATA_HOME/nvim/lazy"
fi
# What the engine is told before a borrowed config runs.
#
# A plugin set meant for daily use keeps itself current in the background:
# it installs what a spec added, fetches its own updates, and installs tool
# binaries at startup. An acceptance run must do none of it -- each is a
# network round trip and a write, on every launch, into the very home this
# run has only borrowed, and one of them puts a notification float on
# screen in the middle of a leg. `lazy.manage` is the whole of the install,
# update, clean and build machinery, which is what makes the borrowed
# `nvim/lazy` link read-only in practice as well as in intent. The modules
# answer as no-ops instead, named one at a time rather than switched off
# wholesale, so a borrowed config still loads every plugin whose paint this
# sweep exists to observe.
PRELUDE_LUA=$RUN_SUPPORT/prelude.lua
cat >"$PRELUDE_LUA" <<'PRELUDE'
local noop = setmetatable({}, { __index = function() return function() end end })
for _, module in ipairs({
  'lazy.manage', 'lazy.manage.checker', 'mason-tool-installer',
}) do
  package.preload[module] = function()
    return noop
  end
end
PRELUDE
ENGINE_PRELUDE=()
[ -z "$SWEEP_DATA" ] || ENGINE_PRELUDE=(--cmd "luafile $PRELUDE_LUA")
# view.toml is composed rather than read in place: the run appends the stub
# agent to it, and a borrowed config is not this script's to write into.
VIEW_TOML=$RUN_SUPPORT/view.toml
if [ -f "$SWEEP_CONFIG/view/view.toml" ]; then
    cp "$SWEEP_CONFIG/view/view.toml" "$VIEW_TOML"
else
    : >"$VIEW_TOML"
fi
# Every session starts through this rather than through the binary, because
# what a leg has to hand tmux is one command line: the engine arguments
# below carry spaces of their own, and a tmux command string re-splits them.
LAUNCHER=$RUN_SUPPORT/launch.sh
{
    printf '#!/usr/bin/env bash\nexec %q --config %q' "$VIEW_BIN" "$VIEW_TOML"
    # guarded rather than left to the expansion: bash's `%q` with no
    # argument at all prints `''`, and an empty argument reaches view as a
    # path to open -- which resolves to the working directory and puts a
    # directory listing where the seeded buffer should be
    [ "${#ENGINE_PRELUDE[@]}" -eq 0 ] || printf ' %q' "${ENGINE_PRELUDE[@]}"
    printf ' "$@"\n'
} >"$LAUNCHER"
chmod +x "$LAUNCHER"

# Every background the assertions turn on, out of the live scheme.
#
# A headless nvim under this run's own XDG environment, which resolves the
# groups exactly as the driven sessions will -- plugins, lazy-loading and
# `ColorScheme` autocommands included -- and then runs the review's own
# `derive`, lifted verbatim out of `REVIEW_SHOW_CHUNK`. That lift is what
# keeps one arithmetic in the tree: the fifth-of-a-foreground blend a
# fg-only diff group is drawn with is written once, in the engine, and read
# back here through the groups it sets rather than repeated in shell.
#
# `-c luafile` rather than `nvim -l`, which runs its script instead of the
# user's config and would answer with nvim's built-in scheme for every
# config on earth.
probe_palette() {
    local derive=$WORK/derive.lua out
    awk '/^local function derive\(\)$/, /^end$/' "$NVIM_API_RS" >"$derive"
    # a floor on what came out, not just its first and last lines: any
    # truncation ending on a column-0 `end` would satisfy those two while
    # lifting an arithmetic missing most of the groups it sets
    grep -qx 'local function derive()' "$derive" &&
        grep -qx 'end' "$derive" &&
        [ "$(grep -c nvim_set_hl "$derive")" -ge 5 ] || {
        printf 'FAIL: no `derive()` to lift out of %s any more, so this sweep would have to re-implement the review'"'"'s own arithmetic\n' \
            "$NVIM_API_RS" >&2
        return 1
    }
    cat >>"$derive" <<'PROBE'
derive()
local out = {}
for _, group in ipairs({
  'Normal', 'CursorLine', 'NormalFloat',
  'ViewReviewAdded', 'ViewReviewRemoved', 'ViewReviewHeader',
  'ViewReviewStale',
}) do
  local hl = vim.api.nvim_get_hl(0, { name = group, link = false })
  local bg = '-'
  if hl.bg ~= nil then
    bg = string.format('%d;%d;%d', math.floor(hl.bg / 65536) % 256,
      math.floor(hl.bg / 256) % 256, hl.bg % 256)
  end
  out[#out + 1] = group .. ' ' .. bg
end
out[#out + 1] = 'mapleader ' .. (vim.g.mapleader or '\\')
io.stdout:write(table.concat(out, '\n') .. '\n')
PROBE
    # stderr kept rather than dropped: a probe that dies for a reason of
    # the config's -- a Lua error on startup, no `nvim` on PATH at all --
    # otherwise reaches the reader only as "answered nothing for Normal",
    # which sends them looking at the group instead of at the config
    out=$(env XDG_CONFIG_HOME="$SWEEP_CONFIG" XDG_DATA_HOME="$DATA_HOME" \
        XDG_STATE_HOME="$STATE_HOME" XDG_CACHE_HOME="$RUN_SUPPORT/cache" \
        nvim --headless ${ENGINE_PRELUDE[@]+"${ENGINE_PRELUDE[@]}"} \
        -c "luafile $derive" -c 'qa!' 2>"$PROBE_ERR") || true
    printf '%s\n' "$out"
}

# One probed group's background, or nothing when the probe did not answer
# for it at all -- which is a run that cannot assert anything and fails
# where it stands rather than comparing against an empty string.
probed_bg() {
    local group="$1" value
    value=$(printf '%s\n' "$PALETTE" | awk -v g="$group" '$1 == g { print $2; found = 1 }
        END { exit !found }') || {
        printf 'FAIL: the probe of %s answered nothing for %s, so no assertion about it could be made\n' \
            "$SWEEP_CONFIG" "$group" >&2
        [ ! -s "$PROBE_ERR" ] || {
            printf '      the probe wrote to stderr:\n' >&2
            tail -5 "$PROBE_ERR" >&2
        }
        return 1
    }
    printf '%s' "$value"
}

PALETTE=$(probe_palette) || exit 1
# What `<leader>` types under this config, read off the config itself: a
# rebound leader (a space, in most configs that rebind it at all) makes
# every default mapping a different keystroke, and a leg typing nvim's
# default would press nothing and report the feature as unreachable.
#
# Read with `sed` rather than through `probed_bg`, whose awk splits on
# whitespace: a space is the very value this exists for, and a field split
# would hand back an empty leader for it.
LEADER=$(printf '%s\n' "$PALETTE" | sed -n 's/^mapleader //p')
[ -n "$LEADER" ] || {
    printf 'FAIL: the probe of %s answered no mapleader, so no leg here could type a default mapping\n' \
        "$SWEEP_CONFIG" >&2
    exit 1
}
NORMAL_BG=$(probed_bg Normal) || exit 1
CURSORLINE_BG=$(probed_bg CursorLine) || exit 1
FLOAT_BG=$(probed_bg NormalFloat) || exit 1
# What a review is drawn with: not the colorscheme's diff groups themselves
# but the four view derives from them at show time (see `REVIEW_SHOW_CHUNK`),
# which carry none of those groups' attributes. A group that defines a
# background hands it over as it stands; one that does not is a fifth of its
# foreground over `Normal`'s background, or over black or white by
# `'background'` when `Normal` has none either. Both cases are the probe's
# to answer, since both are what the engine's own `derive` just did.
REVIEW_ADDED_BG=$(probed_bg ViewReviewAdded) || exit 1
REVIEW_REMOVED_BG=$(probed_bg ViewReviewRemoved) || exit 1
REVIEW_HEADER_BG=$(probed_bg ViewReviewHeader) || exit 1
# what a stale hunk paints with, read by `leg_review_stale` and by the
# distinctness gate below. It is also what `StyleRole::GitModified`
# resolves to in the tree float, so a scheme that let it collide with
# another group fails the gate before any leg has to notice.
REVIEW_STALE_BG=$(probed_bg ViewReviewStale) || exit 1
# A derived group with no background is not a scheme this sweep can read a
# transparent case out of -- it is `derive` having stopped setting one,
# which every review assertion below would then match against the terminal
# default and pass on a review that painted nothing.
for derived in "$REVIEW_ADDED_BG" "$REVIEW_REMOVED_BG" "$REVIEW_HEADER_BG" "$REVIEW_STALE_BG"; do
    [ "$derived" != - ] || {
        printf 'FAIL: a ViewReview* group came back with no background at all, so the review derives nothing to paint with:\n%s\n' \
            "$PALETTE" >&2
        exit 1
    }
done
# An overlay the user cannot tell from the buffer beneath it. A scheme is
# free to leave `Normal` transparent -- that is the terminal's own
# background showing through, and every assertion here can name it -- but a
# float has to own its cells, and view derives its interior from this
# group.
[ "$FLOAT_BG" != - ] || {
    printf 'FAIL: %s leaves NormalFloat with no background, so every overlay view paints reads as the buffer beneath it\n' \
        "$SWEEP_CONFIG" >&2
    exit 1
}
# Every one of them distinct from every other: two that shared a value would
# leave a bleed through an overlay indistinguishable from correct paint, and
# a proposed line indistinguishable from the row it replaces. Two groups
# with no background at all share the loudest value of the lot -- the
# terminal default, which a capture spells the same way for both.
printf '%s\n' "Normal $NORMAL_BG" "CursorLine $CURSORLINE_BG" "NormalFloat $FLOAT_BG" \
    "ViewReviewAdded $REVIEW_ADDED_BG" "ViewReviewRemoved $REVIEW_REMOVED_BG" \
    "ViewReviewHeader $REVIEW_HEADER_BG" "ViewReviewStale $REVIEW_STALE_BG" |
    awk -v scheme="$SWEEP_CONFIG" '
        { if ($2 in owner) { printf "FAIL: the scheme %s resolves to gives %s and %s the same background (%s), so this sweep cannot tell them apart\n", scheme, owner[$2], $1, $2 > "/dev/stderr"; bad = 1 }
          owner[$2] = $1 }
        END { exit bad }' || exit 1

panel_const() {
    local name="$1" value
    value=$(grep -oE "const $name: &str = \"[^\"]+\"" "$PANEL_RS" | sed -E 's/.*"(.*)"/\1/') || true
    [ -n "$value" ] || {
        printf 'FAIL: %s is not a &str constant in %s any more\n' "$name" "$PANEL_RS" >&2
        return 1
    }
    printf '%s' "$value"
}
FOCUSED_TITLE=$(panel_const FOCUSED_TITLE) || exit 1
PANEL_TITLE=$(panel_const TITLE) || exit 1

# The glyph a title too long for its top edge is cut with, read out of the
# framing that appends it.
TRUNCATION_MARK=$(grep -oE "const TRUNCATION_MARK: char = '.*'" "$OVERLAY_RS" |
    sed -E "s/.*= '(.*)'/\1/")
# A mark that is blank, or that a frame is already drawn with, would be on
# screen whether a title was cut or not, and the leg below would report a
# surviving title over an edge that had lost one.
case "$TRUNCATION_MARK" in
'' | ' ' | '-' | '|' | '+' | '─' | '│' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘')
    printf 'FAIL: the truncation mark in %s is %s, which a framed box is full of already, so finding it on screen would prove nothing about a cut title\n' \
        "$OVERLAY_RS" "${TRUNCATION_MARK:-nothing this can read}" >&2
    exit 1
    ;;
esac
# What a narrow top edge keeps of the focused title: its head, which is the
# unfocused title itself. Derived rather than written out, so a retitled
# panel moves this with it -- and checked, because a focused title that
# stopped opening with the panel's name would leave the narrow leg proving
# only that some box is on screen.
case "$FOCUSED_TITLE" in
"$PANEL_TITLE"*) NARROW_FOCUSED_TITLE=$PANEL_TITLE ;;
*)
    printf 'FAIL: the focused title (%s) no longer opens with the panel name (%s), so the head a narrow edge keeps names no panel; give leg_narrow_title its own marker\n' \
        "$FOCUSED_TITLE" "$PANEL_TITLE" >&2
    exit 1
    ;;
esac

# The mark a frozen toast stack sets into its top box's border run, read out
# of the charset that carries it. The sweep drives a UTF-8 tmux pane whose
# box-glyph probe answers yes, so the rounded set is the one on screen.
TOAST_PAUSE_MARK=$(border_glyph ROUNDED pause)
# A mark the frame is already drawn with, or none at all, would be on screen
# whether the stack was frozen or not, and the leg below would report a
# freeze that never happened.
case "$TOAST_PAUSE_MARK" in
'' | ' ' | '-' | '|' | '+' | '─' | '│' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘')
    printf 'FAIL: the pause mark in %s is %s, which a framed box is full of already, so finding it on screen would prove nothing about a frozen stack\n' \
        "$OVERLAY_RS" "${TOAST_PAUSE_MARK:-nothing this can read}" >&2
    exit 1
    ;;
esac

# What each surface writes into its own frame when it is the one that
# opened, read out of the code that writes it.
#
# The screen changing is not evidence that a gesture reached its feature:
# an unmatched `:View` form paints a notice toast, which is a change and a
# framed box both, so a wait for either passes on a key that reached
# nothing but the fallback. What separates them is a word only the intended
# surface paints, so every entry point below waits for its own.
# What the tree's create prompt asks with, read out of the engine call that
# primes `vim.fn.input()` with it: the paste leg has to know the prompt is
# up before it pastes, or it would be pasting at the tree instead.
# How view names a mapping it registered, read out of the format that
# builds it: the entry-points leg tells its own default apart from one the
# driven config bound to the same key by this description, and a leg holding
# a stale copy of it would read every entry point as the config's and skip
# the lot while still reporting green.
DESC_FORMAT=$(grep -oE "desc = string\\.format\\('[^']+'" "$NVIM_API_RS" |
    sed -E "s/.*'(.*)'/\\1/" | head -1) || true
case $DESC_FORMAT in
*%s*%s*) ;;
*)
    printf 'FAIL: %s no longer builds its mapping descriptions from a two-slot format (got %s), so no leg can tell view own key from the config own\n' \
        "$NVIM_API_RS" "${DESC_FORMAT:-nothing}" >&2
    exit 1
    ;;
esac
CREATE_PROMPT=$(grep -A 6 'pub fn tree_create_prompt' "$NVIM_API_RS" |
    grep -oE 'Value::from\("[^"]+"\)' | sed -E 's/.*"(.*)".*/\1/' | head -1) || true
[ -n "$CREATE_PROMPT" ] || {
    printf 'FAIL: %s no longer primes the tree create prompt with a literal question\n' "$NVIM_API_RS" >&2
    exit 1
}
# What a permission question opens with, read out of the format that builds
# it: the caret leg has to know the prompt is up before it reads where the
# keyboard is waiting.
PERMISSION_PROMPT=$(grep -oE 'format!\("Permission requested for' "$PERMISSION_RS" |
    sed -E 's/.*"(.*)/\1/') || true
[ -n "$PERMISSION_PROMPT" ] || {
    printf 'FAIL: the permission prompt is not built from a literal in %s any more\n' \
        "$PERMISSION_RS" >&2
    exit 1
}
HISTORY_TITLE=$(grep -oE 'PaletteView::new\("[^"]+"\)' "$PALETTE_RS" |
    sed -E 's/.*"(.*)".*/\1/' | tail -1) || true
[ -n "$HISTORY_TITLE" ] || {
    printf 'FAIL: the message-history view sets no literal title in %s any more\n' "$PALETTE_RS" >&2
    exit 1
}
# Joined across the two tables that decide it: which `Source` a picker verb
# resolves to, and what title that source paints.
PICKER_MARKERS=$(awk -v surfaces="$SURFACES_RS" -v picker="$PICKER_RS" '
    FILENAME == surfaces && /=> Some\(Source::/ {
        if (match($0, /"[a-z]+"/)) v = substr($0, RSTART + 1, RLENGTH - 2)
        if (match($0, /Source::[A-Za-z]+/)) s = substr($0, RSTART + 8, RLENGTH - 8)
        if (v != "" && s != "") { variant[v] = s; v = ""; s = "" }
    }
    FILENAME == picker && match($0, /Source::[A-Za-z]+[^=]*=> "[^"]+"/) {
        line = substr($0, RSTART, RLENGTH)
        match(line, /Source::[A-Za-z]+/); s = substr(line, RSTART + 8, RLENGTH - 8)
        match(line, /"[^"]+"$/); t = substr(line, RSTART + 1, RLENGTH - 2)
        title[s] = t
    }
    END { for (v in variant) if (variant[v] in title) printf "%s\t%s\n", v, title[variant[v]] }
' "$SURFACES_RS" "$PICKER_RS")

# The marker for one (feature, verb) pair. A pair with none fails the run
# rather than falling back to a weaker check: a surface this script cannot
# name on screen is one it cannot prove was reached, and the sweep going
# quiet over a newly registered key is the hole it exists to close.
marker_for() {
    local feature="$1" verb="$2" marker=""
    # matched on the pair rather than on the feature with a `[ "$verb" = … ]`
    # guard inside the arm: the pair form keeps "which pairs are known" in
    # one column of patterns instead of split between patterns and guards,
    # so an unknown verb on a known feature reaches the diagnostic below
    # by the same path as an unknown feature
    case "$feature/$verb" in
    # the picker's verbs are a table of their own, so the arm is the feature
    picker/*) marker=$(printf '%s\n' "$PICKER_MARKERS" | awk -F'\t' -v v="$verb" '$1 == v { print $2 }') ;;
    # the tree titles itself with the name of the directory it opened on,
    # which is this leg's own working tree
    tree/toggle) marker=$(basename -- "$ROOT") ;;
    notifications/history) marker=$HISTORY_TITLE ;;
    ai/toggle) marker=$PANEL_TITLE ;;
    # not an entry point of its own: the pair form is how every marker in
    # this script is asked for, and a narrow pane's panel is a different
    # thing on screen from the same panel on a wide one
    ai/focus-narrow) marker=$NARROW_FOCUSED_TITLE ;;
    esac
    [ -n "$marker" ] || {
        printf 'FAIL: nothing here knows what the %s %s surface paints, so driving it would prove nothing; give it a marker\n' \
            "$feature" "$verb" >&2
        return 1
    }
    printf '%s' "$marker"
}

# Every default key this build registers, and the feature and verb behind
# it, from the table the engine registers them out of.
ENTRY_POINTS=$(awk '
    /^static DEFAULT_MAPS/ { inside = 1 }
    inside && /feature: "/ { f = $0; sub(/.*feature: "/, "", f); sub(/".*/, "", f) }
    inside && /lhs: "/     { l = $0; sub(/.*lhs: "/, "", l); sub(/".*/, "", l) }
    inside && /verb: "/    { v = $0; sub(/.*verb: "/, "", v); sub(/".*/, "", v); print f, l, v }
    inside && /^\];/ { exit }
' "$MAPPINGS_RS")
# checked against the array's own declared length, because the reader above
# recognizes the three fields by name and in the order they are written: a
# reordered or renamed field would leave it silently short, and a sweep that
# drove four of six entry points would report green over the two it dropped
declared=$(grep -oE '^static DEFAULT_MAPS: \[MappingSpec; [0-9]+\]' "$MAPPINGS_RS" |
    grep -oE '[0-9]+') || true
read_count=$(printf '%s\n' "$ENTRY_POINTS" | grep -c . || true)
if [ -z "$declared" ] || [ "$read_count" != "$declared" ]; then
    printf 'FAIL: %s declares %s default mappings and this read %s of them; the table has changed shape\n' \
        "$MAPPINGS_RS" "${declared:-no}" "$read_count" >&2
    exit 1
fi
# Every key a review installs on the buffer it draws in, read and shape-checked
# the same way as the defaults above (`review_keys_of`, artifacts.sh).
# shellcheck disable=SC2034 # read by `review_key`, which lives in artifacts.sh
REVIEW_KEYS=$(review_keys_of "$MAPPINGS_RS") || exit 1

# The features in registration order with the verb a bare `:View <feature>`
# resolves to. That form is a separate entry point from the key, and the
# only one that reaches a feature without naming what to do: the resolution
# is the feature's first `default_maps()` entry (crates/view-core/src/update/mod.rs),
# which is this table's first row for it.
DEFAULT_VERBS=$(printf '%s\n' "$ENTRY_POINTS" | awk '!seen[$1]++ { print $1, $3 }')

printf 'view acceptance: visual sweep (%s, %s, %sx%s)\n' \
    "${VIEW_BIN#"$REPO_ROOT/"}" "$(nvim --version | head -1)" "$COLS" "$ROWS"
printf '  config %s\n  palette %s\n' "$SWEEP_CONFIG" \
    "$(printf '%s\n' "$PALETTE" | tr '\n' ' ')"
[ -z "$SWEEP_DATA" ] || printf '  plugins borrowed from %s/nvim/lazy\n' "$SWEEP_DATA"

# The resume path the stub holds a reply at, handed to it the way the
# conformance suite hands it: a leg that needs the panel in a known state
# before the agent answers creates the file when it is ready.
#
# Appended to the composed `view.toml` rather than to the config's own,
# which a borrowed config would not have this run's stub agent in and is
# not this script's to edit either. The path stays the same on every launch
# because the theme cache is keyed by it (crates/view/src/theme_cache.rs),
# so a per-leg copy would key a different cache file and every leg would
# run its first frames unthemed.
RESUME_FILE=$(mktemp -u "${TMPDIR:-/tmp}/view-visual-resume-$$-XXXXXX")
printf '\n[ai]\nagent = ["%s", "%s"]\n' "$STUB_BIN" "$RESUME_FILE" >>"$VIEW_TOML"

CURRENT_LEG=theme-cache
SESSION="view-visual-$$-warm"
warm_root=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-warm-XXXXXX")
ROOTS+=("$warm_root")
# this preamble drives its own session rather than going through
# `start_session`, so `fail` is told where its log is the same way
ROOT=$warm_root
mkdir -p "$warm_root/xdg_cache_home"
printf 'warm the theme cache\n' >"$warm_root/scratch.txt"
SESSIONS+=("$SESSION")
tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" -c "$warm_root" \
    "env XDG_CONFIG_HOME=$SWEEP_CONFIG \
         XDG_DATA_HOME=$DATA_HOME \
         XDG_STATE_HOME=$STATE_HOME \
         XDG_CACHE_HOME=$warm_root/xdg_cache_home \
         VIEW_LOG=$warm_root/view.log \
         TERM=xterm-256color COLORTERM=truecolor \
         $LAUNCHER $warm_root/scratch.txt"
wait_for 'warm the theme cache' "$WAIT_SECS" "the warming session's buffer" >/dev/null
watch_view "$SESSION"
# Every assertion below compares a decimal triple, which only a truecolor
# server ever emits. One that stores indexed colors instead answers `48;5;N`
# for every cell, no background matches the fixture's, and the run fails on
# "the box is not themed" -- a true statement about the wrong thing, sending
# a reader after a colorscheme defect that is really a terminal one.
grep -q '48;2;' "$CAP" || {
    fail "the tmux server emits no truecolor background (no 48;2;R;G;B in a themed capture), so no cell here can be matched against the colorscheme" || exit 1
}
# The float background as the cache stores it: the same value the
# assertions below look for on screen, proving the colorscheme reached
# view's own derivation and not merely nvim's.
FLOAT_BG_DECIMAL=$(awk -v triple="$FLOAT_BG" 'BEGIN {
    split(triple, c, ";")
    printf "%d", c[1] * 65536 + c[2] * 256 + c[3]
}')
warm_start=$(now)
while :; do
    # the reader is not `grep -q`: a quiet grep exits at its first match and
    # SIGPIPEs the grep feeding it, which `pipefail` then reports as a
    # failed pipeline -- so the match this loop waits for would read as no
    # match and the leg would time out on a cache that already held it. See
    # `holds`. `-x` keeps the anchoring the pattern carried.
    float_bg_line=$(grep -A 2 -- '^\[chrome.NormalFloat\]' "$STATE_HOME"/view/theme-*.toml 2>/dev/null |
        grep -xE "bg = $FLOAT_BG_DECIMAL") || true
    if [ -n "$float_bg_line" ]; then
        break
    fi
    if ! under "$(elapsed "$warm_start" "$(now)")" "$WAIT_SECS"; then
        fail "the colorscheme's float background never reached the theme cache under $STATE_HOME, so no later leg would have started themed" || exit 1
    fi
    sleep "$POLL"
done
end_session
pass "the colorscheme reaches view's own theme cache (NormalFloat bg $FLOAT_BG)"

leg_entry_points() {
    CURRENT_LEG=entry-points
    start_session entry 'visual sweep seed line'

    # The bare form first, and against a project no trust store has heard
    # of: a feature named without its verb has to resolve to the feature's
    # own entry point before the trust gate runs, or the answer to the
    # prompt re-dispatches a verb no arm matches and the panel never opens.
    mark
    command_line ':View ai'
    wait_change "$REACTION_SECS" "the bare :View ai form" >/dev/null
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    assert_chrome 'the trust prompt'
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the agent panel the answered trust prompt re-dispatches into" >/dev/null
    assert_chrome 'the agent panel'
    dismiss ai
    pass 'a feature named without its verb reaches the panel through the trust prompt'

    # fed from here-documents rather than pipes: a pipeline's loop body runs
    # in a subshell, where a failed assertion would abort that subshell and
    # leave the session list this script cleans up behind incomplete
    local feature lhs verb key marker took
    while read -r feature verb; do
        marker=$(marker_for "$feature" "$verb") || return 1
        mark
        command_line ":View $feature"
        took=$(wait_change "$REACTION_SECS" ":View $feature")
        wait_in_box "$marker" "$REACTION_SECS" "the $feature surface :View $feature resolves to" >/dev/null
        assert_chrome ":View $feature"
        dismiss "$feature"
        pass ":View $feature paints $feature $verb ('$marker') in ${took}s"
    done <<BARE
$DEFAULT_VERBS
BARE

    # The picker's query is a text field like the panel's composer, so the
    # same claim has to hold on a real terminal: keys typed into an open
    # picker land in its query, and the caret is at that query's insertion
    # point rather than on the buffer behind the box.
    local picker_verb picker_marker
    picker_verb=$(printf '%s\n' "$DEFAULT_VERBS" | awk '$1 == "picker" { print $2 }')
    [ -n "$picker_verb" ] || {
        fail 'no picker entry point in DEFAULT_MAPS any more, so there is no query field to check'
        return 1
    }
    picker_marker=$(marker_for picker "$picker_verb") || return 1
    mark
    command_line ':View picker'
    wait_in_box "$picker_marker" "$WAIT_SECS" 'the picker :View picker resolves to' >/dev/null
    send_text QRY
    wait_in_box 'QRY' "$REACTION_SECS" 'a keystroke in the picker query' >/dev/null
    assert_caret_after QRY 'the picker query' || return 1
    dismiss picker

    local desc want_desc pressed=0 skipped=0
    while read -r feature lhs verb; do
        # the pause key is driven by `leg_toast_and_history` instead: it
        # marks a box that is already standing rather than opening one, and
        # the shape below presses its key onto a bare buffer, where there is
        # nothing for the mark to land on and nothing to wait for
        [ "$feature/$verb" != notifications/pause ] || continue
        key=$(tmux_key "$lhs") || return 1
        marker=$(marker_for "$feature" "$verb") || return 1
        desc=$(mapping_desc "$key") || return 1
        # built from the engine's own format string rather than spelled
        # again here: a description that gains a field would otherwise match
        # nothing, every entry point would read as the config's, and the leg
        # would skip its way to green
        want_desc=$(printf "$DESC_FORMAT" "$feature" "$verb")
        if [ "$desc" != "$want_desc" ]; then
            [ "$desc" != -none- ] || {
                fail "nothing maps $lhs in this session, so view's $feature $verb default never registered"
                return 1
            }
            skip "$lhs is this config's own key (\"$desc\"), which outranks view's $feature $verb default; :View $feature above proves the surface"
            skipped=$((skipped + 1))
            continue
        fi
        pressed=$((pressed + 1))
        mark
        send_text "$key"
        took=$(wait_change "$REACTION_SECS" "$lhs")
        wait_in_box "$marker" "$REACTION_SECS" "the $feature $verb surface $lhs is mapped to" >/dev/null
        assert_chrome "$lhs ($feature $verb)"
        dismiss "$feature"
        pass "$lhs paints $feature $verb ('$marker') in ${took}s"
    done <<ENTRIES
$ENTRY_POINTS
ENTRIES
    # a floor under the skips: every entry point being the config's own key
    # is indistinguishable, in a green log, from a leg that stopped matching
    # view's mappings at all -- and this is the leg that proves the keys
    # users actually press
    [ "$pressed" -gt 0 ] || {
        fail "no default key reached view at all ($skipped of $skipped rebound by $SWEEP_CONFIG), so nothing here pressed a key a user would"
        return 1
    }

    end_session
}

leg_toast_and_history() {
    CURRENT_LEG=toast-and-history
    start_session toast 'visual sweep seed line'

    # A toast lands over the cursor row, whose highlight runs the full width
    # of the window beneath it. An overlay that does not own its cells shows
    # that row through itself, which is the shape the interior check reads.
    mark
    command_line ':bogus'
    wait_change "$REACTION_SECS" "a mistyped command" >/dev/null
    wait_in_box 'Not an editor command' "$WAIT_SECS" "the error toast" >/dev/null
    assert_chrome 'the error toast'
    pass 'a mistyped command toasts over the cursor row, opaque'

    # The live negative control for `leg_toast_over_panel`'s overlap check.
    # A bordered box always has an edge left of its own text, so a check that
    # asked only for "some edge to the left" would be answered by the toast
    # itself and would pass over an empty buffer. Here there is nothing under
    # the toast, and the two columns that check compares must therefore be
    # the same one -- if they can differ with no second box on screen, that
    # check is measuring something other than overlap.
    local bare_span bare_left bare_near
    bare_span=$(text_span 'Not an editor command')
    read -r _ _ bare_left bare_near <<<"$bare_span"
    if [ "$bare_left" != "$bare_near" ]; then
        fail "a toast over a bare buffer reports two different box edges left of its text ($bare_left and $bare_near), so the overlap check in leg_toast_over_panel would pass with no panel on screen"
        return 1
    fi
    pass "an unoverlapped toast has only its own frame to its left (column $bare_left)"

    # The pause key, on the standing box above. It freezes the stack's
    # dismissal timing and says so with a mark in the top box's border run --
    # a freeze the user cannot see is indistinguishable from a stuck editor.
    # The mark sits on the frame rather than inside it, so it is read off the
    # cell table; `wait_in_box` pairs vertical edges and only ever sees
    # interior rows. This box is a sticky error, which is the case worth
    # driving live: it takes no slot in the timer queue at all, and the mark
    # still has to be on it, because what it reports is the mode.
    local pause_lhs pause_key toast_row marked
    pause_lhs=$(printf '%s\n' "$ENTRY_POINTS" |
        awk '$1 == "notifications" && $3 == "pause" { print $2 }')
    [ -n "$pause_lhs" ] || {
        fail 'no notifications/pause row in DEFAULT_MAPS any more, so there is no key to freeze the stack with'
        return 1
    }
    pause_key=$(tmux_key "$pause_lhs") || return 1
    if [ "$(mapping_desc "$pause_key")" != "$(printf "$DESC_FORMAT" notifications pause)" ]; then
        skip "$pause_lhs is this config's own key, which outranks view's notifications pause default"
    else
        read -r toast_row _ _ _ <<<"$(text_span 'Not an editor command')"
        mark
        send_text "$pause_key"
        wait_change "$REACTION_SECS" "$pause_lhs" >/dev/null
        settle
        marked=$(LC_ALL=C awk -F'\t' -v r="$((toast_row - 1))" -v g="$TOAST_PAUSE_MARK" \
            '$1 == r && $6 == g { n++ } END { print n + 0 }' "$CELLS")
        if [ "$marked" != 1 ]; then
            fail "the frozen stack's top border run carries $marked '$TOAST_PAUSE_MARK' marks, not 1"
            return 1
        fi
        pass "$pause_lhs marks the frozen stack's top box with '$TOAST_PAUSE_MARK'"

        mark
        send_text "$pause_key"
        wait_change "$REACTION_SECS" "leaving pause" >/dev/null
        settle
        if grep -qF "$TOAST_PAUSE_MARK" "$SCREEN"; then
            fail "the pause mark is still on screen after the freeze was lifted"
            return 1
        fi
        pass 'pressing it again lifts the freeze and takes the mark with it'
    fi

    # and the history browser over it. An error toast is sticky (see
    # `toast::route`), so this is two overlays on screen at once, which is
    # the layered case the reported defect showed a box reading through in.
    mark
    command_line ':View notifications'
    wait_change "$REACTION_SECS" "the message history" >/dev/null
    wait_in_box "$HISTORY_TITLE" "$WAIT_SECS" "the message history overlay" >/dev/null
    assert_chrome 'the message history over a standing toast'
    pass 'the message history opens over that toast, both opaque'

    # Escape closes the history and nothing else: the toast beneath it is an
    # error, and an error that a dismissal aimed at some other overlay takes
    # with it is an error the user never got to read.
    mark
    send_key Escape
    wait_change "$REACTION_SECS" "closing the message history" >/dev/null
    wait_in_box 'Not an editor command' "$WAIT_SECS" "the error toast under the closed history" >/dev/null
    pass 'the error survives the Escape that closes the history over it'

    # and the next one, now that the engine owns the keyboard again, is the
    # deliberate dismissal. `wait_no_box` is the whole assertion: the toast is
    # the only framed thing left on screen.
    send_key Escape
    wait_no_box "$WAIT_SECS" "the dismissed error toast" >/dev/null
    if grep -qF 'Not an editor command' "$SCREEN"; then
        fail 'the dismissed error is still painted on the buffer'
        return 1
    fi
    pass 'a normal-mode Escape takes the sticky error off the screen'

    # dismissed from the screen, not from the log
    mark
    command_line ':View notifications'
    wait_change "$REACTION_SECS" "the message history after the dismissal" >/dev/null
    wait_in_box 'Not an editor command' "$WAIT_SECS" "the dismissed error in the history" >/dev/null
    pass 'the dismissed error is still in the message history'

    end_session
}

# Ephemeral nvim UI over the open agent panel.
#
# The defect this leg exists for was a z-order: the native overlay stack was
# composed last, so the panel -- right-pinned and full height -- covered the
# exact top-right corner the messages box pins itself to, and every toast
# nvim raised while it was open painted underneath it. Nothing failed; the
# notice simply was not on screen. The panel's own review and permission
# notices travel that same Messages layer, so the surface most in need of
# saying something was the one that could not.
#
# Read here rather than in a paint test because the claim is about what a
# terminal was told: a unit test can assert a layer's index, only a capture
# can say the glyphs reached the cells the panel owns.
leg_toast_over_panel() {
    CURRENT_LEG=toast-over-panel
    local toast='Not an editor command' span trow tcol pleft
    start_session overlap 'visual sweep seed line'
    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null

    # out of the composer, not out of the panel: the toast below is raised
    # from nvim's own cmdline, which needs the engine to own the keyboard
    # while the panel stays framed on screen
    send_key Escape
    settle
    if ! holds "$PANEL_TITLE" "$(box_text)"; then
        fail 'Escape closed the panel outright, so there is no panel for a toast to land over'
        return 1
    fi

    mark
    command_line ':bogus'
    wait_change "$REACTION_SECS" "a mistyped command with the panel open" >/dev/null
    wait_for "$toast" "$WAIT_SECS" "the error toast over the open panel" >/dev/null
    assert_chrome 'the error toast over the open panel'

    # the whole point, in four numbers: two distinct box edges left of the
    # toast's text. The nearer one is the toast's own frame -- a bordered box
    # always has one, so a check that only asked for "an edge to the left"
    # would be answered by the toast itself and pass with no panel on screen
    # at all. The further one is the panel, and only a toast drawn over the
    # panel's interior has both. A panel that instead reserved its column and
    # let the toast reflow beside it would leave the two equal, which is the
    # layout this leg exists to tell apart from the one that ships.
    span=$(text_span "$toast")
    if [ -z "$span" ]; then
        fail "the toast text is not in any cell on screen, so the panel is painting over it"
        return 1
    fi
    read -r trow tcol pleft pnear <<<"$span"
    if [ "$pleft" -lt 0 ] || [ "$pleft" -ge "$pnear" ]; then
        fail "the toast on row $trow starts at column $tcol with only its own frame to its left (edges at $pleft and $pnear), so nothing on that row is underneath it and this leg proves nothing"
        return 1
    fi
    pass "a toast paints over the open panel (row $trow, text at column $tcol, its own frame at $pnear, the panel's edge at $pleft)"

    dismiss ai
    end_session
}

leg_panel_typing() {
    CURRENT_LEG=panel-typing
    start_session typing 'visual sweep seed line'
    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null

    # Three bursts rather than one: the reported defect was per keystroke
    # cost, and a composer that painted only its first burst would pass a
    # single one.
    local burst echoed typed tail_mark
    for burst in ALPHA BRAVO CHARLIE; do
        send_text "$burst"
        echoed=$(wait_in_box "$burst" "$REACTION_SECS" "the '$burst' keystroke burst in the composer")
        pass "the composer echoes '$burst' in ${echoed}s"
    done
    assert_chrome 'the agent panel mid-typing'

    # The insertion point itself. The panel had no visible caret at all when
    # this was found live: `composer_cursor` knew where the next character
    # went and nothing placed the terminal's own cursor there, so the caret
    # stayed in nvim's grid while every keystroke landed here. Nothing in a
    # screen dump can see that, which is why it is asserted against tmux's
    # own cursor rather than against cells.
    assert_caret_after 'ALPHABRAVOCHARLIE' 'the composer holding three bursts' || return 1

    # Past the composer's own width by a wide margin, whichever share of the
    # terminal the panel currently takes. The tail is what must still be on
    # screen: the cursor is at the end of the input, and a composer that
    # painted the head of it would be typing where the user cannot look.
    tail_mark=TAILMARK
    typed=$(awk 'BEGIN { for (i = 0; i < 24; i++) printf "wordwordword " }')
    send_text "$typed$tail_mark"
    echoed=$(wait_in_box "$tail_mark" "$WAIT_SECS" "the tail of a prompt longer than the panel is wide")
    assert_chrome 'the agent panel holding a wrapped prompt'
    pass "a $((${#typed} + ${#tail_mark}))-character prompt keeps its tail on screen (${echoed}s)"
    # the caret follows the wrap onto the row the tail was kept on, which is
    # the row the next character is painted on
    assert_caret_after "$tail_mark" 'the composer holding a wrapped prompt' || return 1

    # Out of the panel and back in on the key that put the user there. The
    # defect this exists for was a dead end: Escape leaves the composer
    # without closing the panel, and the toggle read "open" as "close", so
    # the visible panel had no key back into it and only `:View ai open`
    # returned. Driven here rather than in a unit test because "the user is
    # in the composer again" is a claim about what the terminal accepts, not
    # about a flag.
    local ai_key
    ai_key=$(printf '%s\n' "$ENTRY_POINTS" | awk '$1 == "ai" && $3 == "toggle" { print $2 }')
    # named here rather than left to the wait below: an empty key types
    # nothing, and the leg would fail 25s later on "the panel never
    # re-entered" -- a true statement about a key that was never pressed
    [ -n "$ai_key" ] || {
        fail 'no ai/toggle row in DEFAULT_MAPS any more, so there is no key to press; point this leg at whatever replaced it'
        return 1
    }
    ai_key=$(tmux_key "$ai_key") || return 1
    mark
    send_key Escape
    wait_change "$REACTION_SECS" "Escape leaving the composer" >/dev/null
    settle
    if holds "$FOCUSED_TITLE" "$(box_text)"; then
        fail 'Escape left the panel entered, so the round trip below would re-enter a panel it never left'
        return 1
    fi
    if ! holds "$PANEL_TITLE" "$(box_text)"; then
        fail 'Escape closed the panel outright, so there is no un-entered panel for the toggle to return to'
        return 1
    fi
    pass "Escape leaves the panel framed and un-entered ('$PANEL_TITLE')"
    # and the caret leaves with the keyboard: an un-entered panel routes
    # every key to nvim, so a caret still sitting on its composer would be
    # pointing at a field that takes nothing
    assert_caret_outside_a_box 'the panel Escape left un-entered' || return 1

    mark
    send_text "$ai_key"
    echoed=$(wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the panel the toggle re-enters")
    # the title alone would pass on a panel that merely repainted its own
    # border, so the keyboard is proven by a key landing in the composer
    send_text REENTERED
    wait_in_box 'REENTERED' "$REACTION_SECS" "a keystroke in the re-entered composer" >/dev/null
    assert_chrome 'the re-entered agent panel'
    pass "the toggle re-enters an escaped-out-of panel and its composer takes keys again (${echoed}s)"
    # both halves of the round trip: the caret comes back with the keyboard
    assert_caret_after REENTERED 'the re-entered composer' || return 1

    dismiss ai
    end_session
}

# The agent panel on a pane too narrow for its own title.
#
# The defect this leg exists for painted a frame with nothing on its top
# edge: the title was set into the edge only while it fit whole, and the
# panel's share of anything under ~127 columns is a shorter edge than the
# focused title is long. A box that names itself only on a wide terminal is
# a box a laptop user meets anonymous, with no way to tell it from the
# picker or the tree.
leg_narrow_title() {
    CURRENT_LEG=narrow-title
    # scoped to this leg, and read by `start_session` as it starts the pane
    local COLS=$NARROW_COLS marker
    start_session narrow 'visual sweep seed line'

    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt on a $COLS-column pane" >/dev/null
    send_text 'y'
    marker=$(marker_for ai focus-narrow) || return 1
    wait_in_box "$marker" "$WAIT_SECS" "the narrow agent panel's own title" >/dev/null

    # the cut itself, both ways round. Without the first check a pane that
    # turned out wide enough would pass on the whole title and prove nothing
    # about a narrow edge; without the second, a title that had lost its
    # tail silently would read as a whole one.
    if holds "$FOCUSED_TITLE" "$(box_text)"; then
        fail "the $COLS-column pane fits the whole focused title after all, so this leg is not reading a narrow top edge"
        return 1
    fi
    if ! holds "$TRUNCATION_MARK" "$(box_text)"; then
        fail 'the narrow panel names itself with no mark of the cut, so its shortened title reads as the whole one'
        return 1
    fi
    assert_chrome 'the narrow agent panel'
    pass "a $COLS-column pane's panel names itself ('$marker', cut and marked '$TRUNCATION_MARK')"

    dismiss ai
    end_session
}

# A clipboard paste into the entered composer, sent the way a terminal
# sends one: the text wrapped in the paste brackets, which is the only way
# to drive the decode this leg is about. The defect it exists for dropped
# the whole paste on the floor -- the prompt the user pasted produced no
# text, no notice and nothing on screen -- and no unit test above the
# routing could see it, because what a real paste even looks like is
# decided by the terminal, not by the model.
paste_into_pane() {
    local text="$1"
    # the trailing newline is the point: a copied line ends with one, and
    # not reading it as `<CR>` is what bracketed paste exists for. `-r`
    # keeps it a newline -- tmux otherwise translates it to a carriage
    # return, which is a different byte from the one a real paste carries
    printf '%s\n' "$text" >"$WORK/paste.txt"
    tmux load-buffer -b view-paste "$WORK/paste.txt"
    tmux paste-buffer -b view-paste -d -r -p -t "$SESSION"
}

leg_panel_paste() {
    CURRENT_LEG=panel-paste
    local mark=PASTEMARK tree_mark=TREEPASTE echoed copies
    start_session paste 'visual sweep seed line'
    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null

    paste_into_pane "$mark"

    # on the composer's own line, named by the prompt mark: a submitted
    # prompt leaves that line empty and moves its text into the transcript,
    # so finding it here is the same assertion as "nothing was sent"
    echoed=$(wait_in_box "> $mark" "$REACTION_SECS" "the pasted prompt on the composer line")
    # after the settle `assert_chrome` runs, never on the capture the wait
    # above happened to stop on: a mid-frame read is a count of whatever was
    # painted so far
    assert_chrome 'the agent panel holding a pasted prompt'
    copies=$(box_text | grep -cF -- "$mark" || true)
    if [ "$copies" != 1 ]; then
        fail "the pasted prompt is on screen $copies times, so something echoed it into the transcript as well as holding it in the composer"
        return 1
    fi
    pass "a bracketed paste lands in the composer and sends nothing (${echoed}s)"

    # Sent, so what follows starts from an empty composer rather than from
    # whatever this block left in it.
    send_key Enter
    wait_in_box "$USER_MARK$mark" "$WAIT_SECS" "the sent prompt in the transcript" >/dev/null

    # The dogfood complaint's other half: a pasted multi-line prompt read as
    # one condensed row. Its lines have to stand on rows of their own, in
    # the composer and again in the transcript that echoes them -- and the
    # caret has to be on the row the next character goes on, which a
    # trailing newline makes the empty one below the last line.
    local top=PASTETOP end=PASTEEND top_row top_col end_row end_col caret
    paste_into_pane "$top
$end"
    echoed=$(wait_in_box "$end" "$REACTION_SECS" "the pasted prompt's last line")
    assert_chrome 'the agent panel holding a multi-line pasted prompt'
    read -r top_row top_col _ _ <<<"$(text_span "$top")"
    read -r end_row end_col _ _ <<<"$(text_span "$end")"
    if [ -z "$top_row" ] || [ "$end_row" != "$((top_row + 1))" ]; then
        fail "the pasted lines are on rows '$top_row' and '$end_row': a multi-line prompt is condensed into the composer's one row"
        return 1
    fi
    if [ "$end_col" != "$top_col" ]; then
        fail "the pasted lines start at columns $top_col and $end_col, so the second is not indented under the first as one field"
        return 1
    fi
    read -r caret _ _ <<<"$(caret_cell)"
    if [ "$caret" != "$((end_row + 1))" ]; then
        fail "the caret is on row '$caret', not row $((end_row + 1)) -- the empty row the pasted trailing newline opened, where the next character lands"
        return 1
    fi
    pass "a pasted multi-line prompt keeps a row per line with the caret below the last (${echoed}s)"

    # Sent: the entry's own mark opens the first line and the second reads
    # indented under it, on a row of its own.
    send_key Enter
    echoed=$(wait_in_box "$USER_MARK$top" "$REACTION_SECS" "the submitted prompt in the transcript")
    assert_chrome 'the agent panel echoing a multi-line prompt'
    read -r top_row _ _ _ <<<"$(text_span "$top")"
    read -r end_row _ _ _ <<<"$(text_span "$end")"
    if [ "$end_row" != "$((top_row + 1))" ]; then
        fail "the echoed prompt reads on rows '$top_row' and '$end_row': what was sent lost the shape it was pasted with"
        return 1
    fi
    copies=$(box_text | grep -cF -- "$end" || true)
    if [ "$copies" != 1 ]; then
        fail "the sent prompt's last line is on screen $copies times, so the composer kept a copy of what it sent"
        return 1
    fi
    pass "and the submitted prompt keeps that shape in the transcript (${echoed}s)"

    # The other half of a multi-line prompt, and the one no paste can prove:
    # a line break the user types. Alt+Enter is the default binding a
    # terminal reports everywhere (Shift+Enter needs a keyboard protocol
    # that says so), and `<CR>` has to keep sending rather than breaking.
    local typed_top=TYPEDTOP typed_end=TYPEDEND
    send_text "$typed_top"
    send_key M-Enter
    send_text "$typed_end"
    echoed=$(wait_in_box "$typed_end" "$REACTION_SECS" "the typed second line")
    assert_chrome 'the agent panel holding a typed multi-line prompt'
    read -r top_row top_col _ _ <<<"$(text_span "$typed_top")"
    read -r end_row end_col _ _ <<<"$(text_span "$typed_end")"
    if [ -z "$top_row" ] || [ "$end_row" != "$((top_row + 1))" ]; then
        fail "the typed lines are on rows '$top_row' and '$end_row': the line break did not open a composer row"
        return 1
    fi
    if [ "$end_col" != "$top_col" ]; then
        fail "the typed lines start at columns $top_col and $end_col, so the second is not under the first as one field"
        return 1
    fi
    pass "a typed line break opens a composer row (${echoed}s)"

    send_key Enter
    echoed=$(wait_in_box "$USER_MARK$typed_top" "$REACTION_SECS" "the typed prompt in the transcript")
    copies=$(box_text | grep -cF -- "$typed_end" || true)
    if [ "$copies" != 1 ]; then
        fail "the typed prompt's last line is on screen $copies times, so <CR> did not send what was typed"
        return 1
    fi
    pass "and <CR> still sends it (${echoed}s)"

    dismiss ai

    # The other surface with a text input, and the one that cannot take a
    # paste locally at all: the tree's create prompt is nvim blocked inside
    # `vim.fn.input()`, reading its own keys, so pasted text either arrives
    # as keys or is lost. Driven here because nothing below a real engine
    # can answer whether nvim typed it.
    command_line ':View tree'
    wait_in_box "$(basename -- "$ROOT")" "$WAIT_SECS" "the file tree" >/dev/null
    send_text 'a'
    wait_in_box "$CREATE_PROMPT" "$WAIT_SECS" "the tree's create prompt" >/dev/null

    paste_into_pane "$tree_mark"

    echoed=$(wait_in_box "$tree_mark" "$REACTION_SECS" "the pasted name in the create prompt")
    assert_chrome 'the create prompt holding a pasted name'
    pass "a paste into a blocked vim.fn.input() prompt is typed into it (${echoed}s)"

    # One Escape, and the box is gone with it. A `Prompt` overlay retires
    # on the `cmdline_hide` the cancelling key comes back as, not on some
    # later keystroke, so a second key here would hide a regression to the
    # lazy close this asserts against.
    send_key Escape
    wait_gone "$CREATE_PROMPT" "$WAIT_SECS" "the cancelled create prompt" >/dev/null
    pass 'one Escape cancels the blocked prompt and takes its box with it'
    dismiss

    end_session
}

# An agent's proposed edit, drawn in the file it edits.
#
# Nothing else in this tree can see this. The live engine tests read the
# marks back through nvim's own API -- attributes, not pixels -- and no
# paint test sees the buffer at all: view composites what the engine sends
# it as ordinary grid traffic, so a decoration that never reached the grid,
# or reached it stripped of its highlight, would fail no assertion above
# this file. The claim here is the user's own: the proposal is visible,
# where the code is, in colors view derived from the colorscheme's own diff
# groups.
# The proposal both review legs start from, and the row it replaces.
REVIEW_PROPOSED='+BETA'
REVIEW_REPLACED='beta'

# Drives the stub to propose an edit and waits until it is drawn in the
# buffer, leaving the cursor where the review put it -- on the hunk.
open_inline_review() {
    start_session review 'visual sweep seed line'
    # The file the stub's `propose` offers edits to, seeded with what its own
    # `oldText` claims it holds. Deliberately not the file the session
    # opened: this is also the case where the review has to bring up a
    # buffer no window was showing.
    printf 'alpha\nbeta\ngamma\n' >"$ROOT/view-ai-stub-diff.txt"

    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null
    # the composer, not the `:` line `command_line` is named for: the panel
    # has focus here and the prompt is what it takes
    send_text 'propose'
    send_key Enter
    wait_for "$REVIEW_PROPOSED" "$WAIT_SECS" "the agent's proposed line" >/dev/null
}

leg_inline_review() {
    CURRENT_LEG=inline-review
    local proposed=$REVIEW_PROPOSED replaced=$REVIEW_REPLACED header='hunk 1/1' key
    open_inline_review

    # The proposed line and the header naming the keys are virtual lines --
    # nvim's, drawn between the buffer's own rows, which is why the text
    # beneath them can stay untouched.
    assert_buffer_bg "$proposed" "$REVIEW_ADDED_BG" "the proposed line ('$proposed')" || return 1
    assert_buffer_bg "$header" "$REVIEW_HEADER_BG" "the current hunk's header ('$header')" || return 1

    # The row the proposal would replace, read with the cursor moved off it:
    # `CursorLine` runs the full width of the window and the review puts the
    # cursor on the hunk, so a row read where the cursor sits would be
    # answering for whichever of the two won rather than for the decoration.
    send_text 'G'
    assert_buffer_bg "$replaced" "$REVIEW_REMOVED_BG" "the row the hunk replaces ('$replaced')" || return 1

    # The panel beside it is still a panel: a decoration that leaked its own
    # colors into the overlay stack would be a compositing defect, not a
    # review.
    assert_chrome 'the agent panel beside a decorated buffer' || return 1

    # Decided with the key the review installs, and then gone: no cell left
    # on screen carries any of the three groups the decoration is drawn
    # with. A namespace that outlived its review would leave a proposal
    # painted over the text it had already become.
    key=$(review_key accept) || return 1
    mark
    send_text "$key"
    wait_change "$REACTION_SECS" "the accepted hunk" >/dev/null
    settle
    # The three values this hunts are also what `StyleRole::GitAdded` and its
    # neighbours resolve to in the tree float, since both read the same diff
    # groups. No tree is open in this leg, so the overlap costs nothing --
    # a leg that opened one would have to scope the scan to the buffer rows.
    local stragglers
    stragglers=$(LC_ALL=C awk -F'\t' -v a="$REVIEW_ADDED_BG" -v d="$REVIEW_REMOVED_BG" \
        -v t="$REVIEW_HEADER_BG" '$3 == a || $3 == d || $3 == t { print; n++ }
        END { exit !n }' "$CELLS") && {
        printf '%s\n' "$stragglers" | head -6 >&2
        fail 'the accepted review is still drawn: cells on screen carry the decoration groups'
        return 1
    }
    if ! grep -qF 'BETA' "$SCREEN"; then
        fail 'the accepted text is not on screen, so the decoration went and the write did not'
        return 1
    fi
    pass 'the accepted review takes its whole decoration off the screen with it'

    dismiss ai
    end_session
}

# The one review color no other leg paints: a hunk the buffer moved out
# from under, still drawn, on `ViewReviewStale`.
#
# It is the value a colorscheme is most likely to get wrong on its own --
# the other three derive from `DiffDelete`/`DiffAdd`/`DiffText` while this
# one comes from `DiffChange`, which habamax states with no foreground and
# a transparent config leaves at the terminal default. The probe decides
# what it should be here, as it does for the others; nothing in this file
# names a color.
leg_review_stale() {
    CURRENT_LEG=review-stale
    local staled="$REVIEW_REPLACED STALE"
    open_inline_review

    # what makes a hunk stale: the user types into the very rows it was
    # computed against, so its `oldText` no longer names what is there.
    # `2G` is the row the proposal replaces -- the review opened with the
    # cursor on it already, but saying which row is what keeps this leg
    # readable when the payload grows a second hunk.
    send_text '2G'
    send_text 'A STALE'
    send_key Escape
    wait_for "$staled" "$WAIT_SECS" "the typed-over row" >/dev/null

    # read with the cursor moved off, for the reason the removed-row read in
    # `leg_inline_review` gives: `CursorLine` runs the full width and would
    # be answering instead of the decoration
    send_text 'G'
    assert_buffer_bg "$staled" "$REVIEW_STALE_BG" "the staled row ('$staled')" || return 1

    dismiss ai
    end_session
}

# The sidebar resize chord, the way a user arriving from nvim types it.
#
# `<S-Right>`/`<S-Left>` are the other half of the same binding and are
# swept by the entry-point leg; this one exists for the chord, which is a
# two-key gesture with state between the presses and two ways to be wrong
# that no single-key test can see: a follower that finishes nothing must be
# handled as though it had been typed alone, and a prefix armed in a sidebar
# that then loses focus must not still be waiting minutes later to spend the
# next `>` on a width. The width itself is state no capture holds, so every
# assertion below reads the panel's own frame edge instead.
leg_resize_chord() {
    CURRENT_LEG=resize-chord
    start_session chord 'visual sweep seed line'
    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null

    local opened wider narrower armed dropped start
    opened=$(panel_edge)
    [ -n "$opened" ] || {
        fail 'the entered panel has no frame edge on screen, so nothing here can read its width'
        return 1
    }

    send_key 'C-w'
    send_text '>'
    # polled rather than read once: the chord crosses the engine and comes
    # back as a repaint, and a single capture right after the second key
    # would be reading the frame before the width moved
    start=$(now)
    while :; do
        wider=$(panel_edge)
        [ -n "$wider" ] && [ "$wider" != "$opened" ] && break
        if ! under "$(elapsed "$start" "$(now)")" "$REACTION_SECS"; then
            fail "'<C-w>>' left the panel edge at column $opened after ${REACTION_SECS}s, so the chord reached no resize"
            return 1
        fi
        sleep "$POLL"
    done
    [ "$wider" -lt "$opened" ] || {
        fail "'<C-w>>' moved the panel edge from column $opened to $wider, which is a narrower panel, not a wider one"
        return 1
    }
    pass "'<C-w>>' widens the panel (left edge column $opened -> $wider)"

    send_key 'C-w'
    send_text '<'
    start=$(now)
    while :; do
        narrower=$(panel_edge)
        [ -n "$narrower" ] && [ "$narrower" != "$wider" ] && break
        if ! under "$(elapsed "$start" "$(now)")" "$REACTION_SECS"; then
            fail "'<C-w><' left the panel edge at column $wider after ${REACTION_SECS}s, so the chord reached no resize"
            return 1
        fi
        sleep "$POLL"
    done
    [ "$narrower" -gt "$wider" ] || {
        fail "'<C-w><' moved the panel edge from column $wider to $narrower, which is a wider panel, not a narrower one"
        return 1
    }
    pass "'<C-w><' narrows the panel back (left edge column $wider -> $narrower)"

    # A follower that finishes no chord: typed alone is what it means, and
    # the composer is where "typed alone" lands. Without this the keys the
    # whole binding exists for would be the one class a waiting prefix
    # locks out.
    send_key 'C-w'
    send_text 'FELLTHROUGH'
    wait_in_box 'FELLTHROUGH' "$REACTION_SECS" 'a follower that finishes no chord' >/dev/null
    armed=$(panel_edge)
    [ "$armed" = "$narrower" ] || {
        fail "typing after '<C-w>' moved the panel edge from column $narrower to $armed, so a fall-through spent itself on a width"
        return 1
    }
    pass "a follower finishing no chord types into the composer and resizes nothing (edge still column $narrower)"

    # And the prefix does not outlive the focus it was armed in. `<Esc>`
    # cannot stand in for that case: it is itself a keystroke, and every
    # keystroke consumes the prefix on its way through the resolver whether
    # the panel keeps focus or not. The case that needs the drop is focus
    # leaving with no key involved at all -- a review landing hands the
    # keyboard back to the buffer it is drawn in -- after which a prefix
    # still waiting would meet the next `>` the user types on coming back
    # and spend it on a width instead of putting it in the prompt.
    send_key Enter
    printf 'alpha\nbeta\ngamma\n' >"$ROOT/view-ai-stub-diff.txt"
    rm -f "$RESUME_FILE"
    # the stub holds this review until the resume file appears, so the
    # prefix below is armed inside a window this leg closes rather than one
    # it races the agent for
    send_text 'propose-when-released'
    # The transcript's own entry is the proof view has drained the
    # keystrokes before the prefix: without it the `settle` below is the
    # only thing standing between the prefix and a review that lands first,
    # and a prefix the panel never read makes every assertion after it pass
    # on a case the leg did not set up. The mark is what makes it proof --
    # the text alone is on screen from the moment it is typed, in the
    # composer, whether or not the panel ever took it as a prompt.
    #
    # And the key is repeated rather than sent once: the panel runs one
    # turn at a time, the `FELLTHROUGH` above is a turn, and a `<CR>`
    # arriving while it is still in flight is refused with the text left
    # in the composer. A repeat costs nothing -- `<CR>` on the empty
    # composer a submit leaves behind is the same no-op.
    start=$(now)
    while :; do
        send_key Enter
        capture
        matches "($TAKEN_MARKS) propose-when-released" "$(box_text)" && break
        if ! under "$(elapsed "$start" "$(now)")" "$WAIT_SECS"; then
            fail "the prompt whose review this leg holds was never taken: nothing in the transcript opens 'propose-when-released' with one of '$TAKEN_MARKS' after ${WAIT_SECS}s"
            return 1
        fi
        sleep "$POLL"
    done
    send_key 'C-w'
    settle
    # the panel paints no pending-chord indicator, so this reads the
    # negative only: nvim is not the one holding a prefix. That the panel
    # arms one is what the FELLTHROUGH step above proves.
    if grep -qF '^W' "$SCREEN"; then
        fail 'nvim is showing a pending ^W of its own, so the prefix this leg armed did not belong to the panel'
        return 1
    fi
    touch "$RESUME_FILE"
    wait_for '+BETA' "$WAIT_SECS" 'the review the agent proposed' >/dev/null
    settle
    # removed here rather than at the leg's end, because every exit below
    # this point is a failing one and each of them used to leave the file
    # behind: the stub reads it as "the held review may resume", so a later
    # leg that stalls would find its own stall already released and the
    # second failure would be diagnosed as the first one's shape
    rm -f "$RESUME_FILE"

    local ai_key
    ai_key=$(printf '%s\n' "$ENTRY_POINTS" | awk '$1 == "ai" && $3 == "toggle" { print $2 }')
    [ -n "$ai_key" ] || {
        fail 'no ai/toggle row in DEFAULT_MAPS any more, so there is no key back into the panel'
        return 1
    }
    ai_key=$(tmux_key "$ai_key") || return 1
    send_text "$ai_key"
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" 'the panel the toggle re-enters' >/dev/null
    send_text '>DROPPED'
    wait_in_box '>DROPPED' "$REACTION_SECS" "the '>' typed after focus came back" >/dev/null
    dropped=$(panel_edge)
    [ "$dropped" = "$narrower" ] || {
        fail "a '>' typed after a review took focus from the panel moved the edge from column $narrower to $dropped, so the chord prefix outlived the focus it was armed in"
        return 1
    }
    pass "a chord prefix armed in the panel is dropped when a review takes the focus (edge still column $narrower)"

    assert_chrome 'the resized agent panel'
    dismiss ai
    end_session
}

# Where the keyboard is waiting while a permission question stands.
#
# The composer swallows every printable until the question is answered, so a
# caret left on it -- wearing the bar an editor uses to say *type here* --
# invites exactly the keystrokes the panel is about to eat. The contract is
# that it stands on the digit that answers instead, and that is terminal
# state: no screen dump of this panel can tell the two apart, which is why
# it is read from tmux and joined to the capture here rather than pinned in
# a surface test alone.
leg_permission_caret() {
    CURRENT_LEG=permission-caret
    start_session permission 'visual sweep seed line'
    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null

    send_text 'ask'
    send_key Enter
    wait_in_box "$PERMISSION_PROMPT" "$WAIT_SECS" 'the permission question' >/dev/null

    local row col flag glyph start
    settle
    read -r row col flag <<<"$(caret_cell)"
    [ "$flag" = 1 ] || {
        fail "the caret is hidden (flag '$flag') while a permission question stands, so the user is shown no waiting key at all"
        return 1
    }
    glyph=$(caret_glyph)
    [ "$glyph" = 1 ] || {
        fail "the caret stands on '${glyph:-nothing}' at row $row col $col, not on the '1' that answers the first option"
        return 1
    }
    pass "a pending permission puts the caret on the digit that answers it (row $row col $col, glyph '1')"

    # The other half of the same promise: the keys the caret is not offering
    # go nowhere. A composer that took them would be collecting a prompt
    # with no caret in it.
    send_text 'ZZTOP'
    sleep "$POLL"
    settle
    if holds ZZTOP "$(box_text)"; then
        fail 'a printable typed at a pending permission reached the composer, which the caret is not pointing at'
        return 1
    fi
    pass 'printables typed at a pending permission reach no composer'

    send_text '1'
    start=$(now)
    while :; do
        settle
        holds "$PERMISSION_PROMPT" "$(box_text)" || break
        if ! under "$(elapsed "$start" "$(now)")" "$WAIT_SECS"; then
            fail "the permission question is still up ${WAIT_SECS}s after the digit the caret was standing on"
            return 1
        fi
        sleep "$POLL"
    done
    send_text 'BACKINTHECOMPOSER'
    wait_in_box 'BACKINTHECOMPOSER' "$REACTION_SECS" 'the composer after the answer' >/dev/null
    assert_caret_after 'BACKINTHECOMPOSER' 'the composer once the question is answered' || return 1

    assert_chrome 'the agent panel after a permission answer'
    dismiss ai
    end_session
}

# A transcript entry re-wrapped under a live resize.
#
# The panel caches the rows it wrapped, and a narrower panel that kept them
# would paint an entry off its own right edge -- the text still there, in a
# width that no longer exists. Pinned as a cache drop and an anchor clamp in
# unit tests; what neither can see is a real SIGWINCH arriving mid-frame and
# the rows the terminal is left holding, which is what this reads.
leg_transcript_reflow() {
    CURRENT_LEG=transcript-reflow
    start_session reflow 'visual sweep seed line'
    command_line ':View ai'
    wait_in_box 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_in_box "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null

    # Long enough to wrap several times at this width, so a re-wrap has rows
    # to add rather than a single row to leave alone.
    local head=REFLOWHEAD tail=REFLOWTAIL body wide narrow start
    body=$(awk 'BEGIN { for (i = 0; i < 14; i++) printf "reflowing " }')
    send_text "$head $body$tail"
    send_key Enter
    wait_in_box "$USER_MARK$head" "$WAIT_SECS" "the submitted entry" >/dev/null
    holds "$tail" "$(box_text_joined)" || {
        fail "the submitted entry is on screen without its own tail ('$tail'), so it was cut rather than wrapped"
        return 1
    }
    wide=$(entry_rows "$head")
    [ -n "$wide" ] && [ "$wide" -gt 1 ] || {
        fail "the submitted entry spans ${wide:-no} row(s) at $COLS columns, so it is not wrapped and a re-wrap would prove nothing"
        return 1
    }
    pass "the transcript entry wraps over $wide rows at $COLS columns"

    tmux resize-window -t "$SESSION" -x "$NARROW_COLS" -y "$ROWS"
    start=$(now)
    while :; do
        narrow=$(entry_rows "$head")
        [ -n "$narrow" ] && [ "$narrow" != "$wide" ] && break
        if ! under "$(elapsed "$start" "$(now)")" "$WAIT_SECS"; then
            fail "the entry still spans ${narrow:-no readable} row(s) ${WAIT_SECS}s after the pane narrowed to $NARROW_COLS columns, so the panel is painting rows it wrapped for a width that is gone"
            return 1
        fi
        sleep "$POLL"
    done
    [ "$narrow" -gt "$wide" ] || {
        fail "the entry went from $wide rows to $narrow on a narrower pane, which is fewer rows for less width"
        return 1
    }
    holds "$tail" "$(box_text_joined)" || {
        fail "the entry lost its tail ('$tail') when the pane narrowed, so the re-wrap dropped text rather than re-laying it"
        return 1
    }
    # the head is the anchor: an entry re-wrapped into more rows than the
    # panel has left would push its own beginning off the top, and the user
    # would watch the thing they were reading leave the screen on a resize
    if ! holds "$head" "$(box_text)"; then
        fail "the entry's first row ('$head') left the panel when the pane narrowed, so the resize scrolled the user off what they were reading"
        return 1
    fi
    pass "a live narrowing re-wraps the entry ($wide rows -> $narrow) and keeps its anchor row on screen"

    assert_chrome 'the agent panel after a live resize'
    dismiss ai
    end_session
}

LEGS=(leg_entry_points leg_toast_and_history leg_toast_over_panel leg_panel_typing
    leg_panel_paste leg_narrow_title leg_inline_review leg_resize_chord
    leg_permission_caret leg_transcript_reflow leg_review_stale)
if [ "$#" -eq 0 ]; then
    selected=("${LEGS[@]}")
else
    selected=()
    for want in "$@"; do
        # spelled out rather than left to the subscript: bash reads a
        # negative index from the end of the array, so `0` and `xyz` both
        # resolve to the last leg and would report a green run of a leg
        # nobody asked for
        case $want in
        '' | *[!0-9]*) leg="" ;;
        *) [ "$want" -ge 1 ] && leg=${LEGS[$((want - 1))]:-} || leg="" ;;
        esac
        [ -n "$leg" ] || {
            printf 'FAIL: there is no leg %s (1..%s)\n' "$want" "${#LEGS[@]}" >&2
            exit 1
        }
        selected+=("$leg")
    done
fi
for leg in "${selected[@]}"; do "$leg"; done

printf 'visual sweep: %s of %s legs green\n' "${#selected[@]}" "${#LEGS[@]}"
