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
# The three background values the assertions turn on are the fixture's own
# (scripts/acceptance/fixtures/themed) and are read out of it here, so a
# retuned fixture moves the assertions with it rather than leaving them
# matching a color nothing paints any more. The entry points driven are read
# out of `DEFAULT_MAPS` for the same reason: a feature that gains a key
# gains a leg here on the same commit, and one whose key stops doing
# anything fails loudly instead of quietly going unswept.
#
# Needs `tmux` and the pinned `nvim`. No network and no agent: the panel is
# opened against the stub agent the conformance leg already builds, since
# what is under test is the paint, never the conversation.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/acceptance/artifacts.sh
. "$SCRIPT_DIR/artifacts.sh"
VIEW_BIN=${VIEW_BIN:-$TARGET_ROOT/release/view}
STUB_BIN=${STUB_BIN:-$TARGET_ROOT/release/view-ai-stub-agent}
FIXTURE=$SCRIPT_DIR/fixtures/themed
COLORSCHEME=$FIXTURE/nvim/colors/view-dracula.lua
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

# Where the framed boxes on screen are, shared by everything that has a
# question about their insides.
#
# A row is scanned for border glyphs and they are paired left to right, so
# two boxes side by side (a sidebar and a centered overlay, a panel and a
# toast) are two independent spans rather than one span swallowing the
# buffer between them. An odd border glyph out -- a box clipped by the
# pane's edge -- ends the row's pairing rather than reaching to its end.
BOX_AWK='
function is_edge(g) {
    return (g == "\342\224\202" || g == "\342\224\214" || g == "\342\224\220" ||
            g == "\342\224\224" || g == "\342\224\230")
}
# a box whose side is covered by a box drawn over it leaves the row with a
# corner where its other side should be, and pairing across that would
# sweep the buffer between two boxes as if it were an interior.
# Under-checking such a row is the safe way to be wrong; reporting the
# buffer as a bleed is not.
function clipped(l, r) {
    return (l == "\342\224\220" || l == "\342\224\230" ||
            r == "\342\224\214" || r == "\342\224\224")
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
        grep -q '┌' "$SCREEN" || return 0
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what is still framed on screen after ${budget}s"
            return 1
        fi
        sleep "$POLL"
    done
}

# The text of every framed box in the last capture.
box_text() { LC_ALL=C awk "$BOX_AWK$BOX_TEXT_AWK" "$CELLS"; }

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
    mkdir -p "$ROOT/xdg_data_home" "$ROOT/xdg_cache_home"
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
        "env XDG_CONFIG_HOME=$CONFIG_HOME \
             XDG_DATA_HOME=$ROOT/xdg_data_home \
             XDG_STATE_HOME=$STATE_HOME \
             XDG_CACHE_HOME=$ROOT/xdg_cache_home \
             VIEW_LOG=$ROOT/view.log \
             TERM=xterm-256color COLORTERM=truecolor \
             $VIEW_BIN $ROOT/scratch.txt"

    wait_for "$seed" "$WAIT_SECS" "the seeded buffer" >/dev/null || return 1
    watch_view "$SESSION" || return 1
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

# The `#rrggbb` a fixture group gives `field` (`bg` or `fg`). Matched on
# the field's own key rather than on where the hex sits in the call, since
# a group written `bg` before `fg` would otherwise read one as the other.
# The key is anchored on the `{` or `,` that opens it rather than on `\b`,
# which BSD `sed -E` reads as a literal `b` -- a word class there would
# match nothing on macOS and fail this sweep for a fixture that is fine.
fixture_hex() {
    local group="$1" field="$2" hex
    hex=$(sed -nE "s/.*'$group', [^}]*[{,][[:space:]]*$field = '(#[0-9a-f]{6})'.*/\1/p" \
        "$COLORSCHEME" | tail -1) || true
    if [ -z "$hex" ]; then
        printf 'FAIL: %s has no %s in %s any more\n' "$group" "$field" "$COLORSCHEME" >&2
        return 1
    fi
    printf '%s' "$hex"
}

# A fixture group's background as the decimal triple a truecolor escape
# spells it with. Read rather than repeated so a retuned fixture cannot
# leave the assertions matching a color nothing paints.
fixture_bg() {
    local hex
    hex=$(fixture_hex "$1" bg) || return 1
    printf '%d;%d;%d' "0x${hex:1:2}" "0x${hex:3:2}" "0x${hex:5:2}"
}

# What the review derives from a diff group that has no background to hand
# over: a fifth of its foreground over `Normal`'s background, which is the
# arithmetic `REVIEW_SHOW_CHUNK`'s `derive` does inside nvim. Integer
# rounding, in the shell rather than in awk, so the leg needs no more of a
# host than every other assertion here does.
review_blend() {
    local hex base out= i c b
    hex=$(fixture_hex "$1" fg) || return 1
    base=$(fixture_hex Normal bg) || return 1
    for i in 1 3 5; do
        c=$((16#${hex:i:2}))
        b=$((16#${base:i:2}))
        out="${out:+$out;}$(((10 * b + 2 * (c - b) + 5) / 10))"
    done
    printf '%s' "$out"
}

NORMAL_BG=$(fixture_bg Normal) || exit 1
CURSORLINE_BG=$(fixture_bg CursorLine) || exit 1
FLOAT_BG=$(fixture_bg NormalFloat) || exit 1
# What a review is drawn with: not the colorscheme's diff groups themselves
# but the five view derives from them at show time (see `REVIEW_SHOW_CHUNK`),
# which carry none of those groups' attributes. A group that defines a
# background hands it over as it stands, so those read back as the
# fixture's own values -- which is why the fixture keeps `DiffDelete`
# foreground-only and `reverse`, dracula's real shape: its derived value is
# a color no group in the scheme defines, so a review that reverted to
# painting with the diff groups themselves fails this leg rather than
# matching it.
REVIEW_ADDED_BG=$(fixture_bg DiffAdd) || exit 1
REVIEW_REMOVED_BG=$(review_blend DiffDelete) || exit 1
REVIEW_HEADER_BG=$(fixture_bg DiffText) || exit 1
# read for the gate below rather than for a leg: it is what a stale hunk
# paints with and what `StyleRole::GitModified` resolves to in the tree
# float, so a fixture that let it collide with another group would be found
# by whichever leg reads it next rather than here
REVIEW_STALE_BG=$(fixture_bg DiffChange) || exit 1
# Every one of them distinct from every other: two that shared a value would
# leave a bleed through an overlay indistinguishable from correct paint, and
# a proposed line indistinguishable from the row it replaces.
printf '%s\n' "Normal $NORMAL_BG" "CursorLine $CURSORLINE_BG" "NormalFloat $FLOAT_BG" \
    "ViewReviewAdded $REVIEW_ADDED_BG" "ViewReviewRemoved $REVIEW_REMOVED_BG" \
    "ViewReviewHeader $REVIEW_HEADER_BG" "ViewReviewStale $REVIEW_STALE_BG" |
    awk -v scheme="$COLORSCHEME" '
        { if ($2 in owner) { printf "FAIL: %s gives %s and %s the same background (%s), so this sweep cannot tell them apart\n", scheme, owner[$2], $1, $2 > "/dev/stderr"; bad = 1 }
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

# The config and state a real returning user has: a session that has already
# run once against this colorscheme and left its derived theme on disk. Both
# are built here rather than per leg because the cache is keyed by the
# resolved config path (crates/view/src/theme_cache.rs), so the path must be
# the same one on every launch that is meant to hit it.
CONFIG_HOME=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-config-XXXXXX")
STATE_HOME=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-state-XXXXXX")
ROOTS+=("$CONFIG_HOME" "$STATE_HOME")
cp -R "$FIXTURE/nvim" "$FIXTURE/view" "$CONFIG_HOME/"
# The resume path the stub holds a reply at, handed to it the way the
# conformance suite hands it: a leg that needs the panel in a known state
# before the agent answers creates the file when it is ready.
RESUME_FILE=$(mktemp -u "${TMPDIR:-/tmp}/view-visual-resume-$$-XXXXXX")
printf '\n[ai]\nagent = ["%s", "%s"]\n' "$STUB_BIN" "$RESUME_FILE" >>"$CONFIG_HOME/view/view.toml"

CURRENT_LEG=theme-cache
SESSION="view-visual-$$-warm"
warm_root=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-warm-XXXXXX")
ROOTS+=("$warm_root")
# this preamble drives its own session rather than going through
# `start_session`, so `fail` is told where its log is the same way
ROOT=$warm_root
mkdir -p "$warm_root/xdg_data_home" "$warm_root/xdg_cache_home"
printf 'warm the theme cache\n' >"$warm_root/scratch.txt"
SESSIONS+=("$SESSION")
tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" -c "$warm_root" \
    "env XDG_CONFIG_HOME=$CONFIG_HOME \
         XDG_DATA_HOME=$warm_root/xdg_data_home \
         XDG_STATE_HOME=$STATE_HOME \
         XDG_CACHE_HOME=$warm_root/xdg_cache_home \
         VIEW_LOG=$warm_root/view.log \
         TERM=xterm-256color COLORTERM=truecolor \
         $VIEW_BIN $warm_root/scratch.txt"
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

    while read -r feature lhs verb; do
        key=$(tmux_key "$lhs") || return 1
        marker=$(marker_for "$feature" "$verb") || return 1
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

    # the prompt first (Escape cancels the input, creating nothing), then
    # the tree that raised it
    send_key Escape
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
leg_inline_review() {
    CURRENT_LEG=inline-review
    local proposed='+BETA' replaced='beta' header='hunk 1/1' key
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
    wait_for "$proposed" "$WAIT_SECS" "the agent's proposed line" >/dev/null

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
    leg_permission_caret leg_transcript_reflow)
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
