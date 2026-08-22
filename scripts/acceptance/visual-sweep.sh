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
VIEW_BIN=${VIEW_BIN:-$REPO_ROOT/target/release/view}
STUB_BIN=${STUB_BIN:-$REPO_ROOT/target/release/view-ai-stub-agent}
FIXTURE=$SCRIPT_DIR/fixtures/themed
COLORSCHEME=$FIXTURE/nvim/colors/view-dracula.lua
MAPPINGS_RS=$REPO_ROOT/crates/view-core/src/native/mappings.rs
PANEL_RS=$REPO_ROOT/crates/view-core/src/native/ai_panel/mod.rs

# The pane every leg reads. The width is what the agent panel's own title
# needs: the panel takes a fixed share of the terminal, its focused title is
# the longest label any overlay here sets into a border, and a narrower pane
# drops that title rather than truncating it -- which would leave the one
# wait that proves the panel entered matching nothing.
COLS=140
ROWS=44
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

SESSIONS=()
ROOTS=()
SESSION=""
ROOT=""
CURRENT_LEG=startup
DUMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-sweep-XXXXXX")
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
    for session in ${SESSIONS[@]+"${SESSIONS[@]}"}; do
        tmux kill-session -t "$session" 2>/dev/null || true
    done
    # `tmux kill-session` returns before the pane's own process is gone, and
    # a removal that overtakes a still-live `view` walks past directories it
    # then re-creates
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pgrep -f "$VIEW_BIN" >/dev/null 2>&1 || break
        sleep 0.2
    done
    for root in ${ROOTS[@]+"${ROOTS[@]}"}; do
        [ -n "$root" ] && rm -rf "$root"
    done
    rm -rf "$WORK"
    if [ "$code" -eq 0 ] || [ -z "$(ls -A "$DUMP_DIR" 2>/dev/null)" ]; then
        rm -rf "$DUMP_DIR"
    else
        printf '      pane dumps kept in %s\n' "$DUMP_DIR" >&2
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }
under() { awk -v v="$1" -v hi="$2" 'BEGIN { exit !(v <= hi) }'; }

# One screen cell per line: row, column, background, reverse flag, glyph,
# tab separated.
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
        if (v == 0) { bg = "-"; rev = 0 }
        else if (v == 7) rev = 1
        else if (v == 27) rev = 0
        else if (v == 49) bg = "-"
        else if (v == 48) {
            if (a[k + 1] + 0 == 2) { bg = a[k + 2] ";" a[k + 3] ";" a[k + 4]; k += 4 }
            else if (a[k + 1] + 0 == 5) { bg = "p" a[k + 2]; k += 2 }
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
        printf "%d\t%d\t%s\t%d\t%s\n", row, col, bg, rev, glyph
        col++
        i = j
    }
}'

# Every framed box on screen, checked cell by cell from its left border to
# its right one.
#
# A row is scanned for border glyphs and they are paired left to right, so
# two boxes side by side (a sidebar and a centered overlay, a panel and a
# toast) are two independent spans rather than one span swallowing the
# buffer between them. An odd border glyph out -- a box clipped by the
# pane's edge -- ends the row's pairing rather than reaching to its end.
CHROME_AWK='
function note(row, col, what) {
    findings++
    if (findings <= 6) printf "      row %d col %d carries %s\n", row, col, what
}
BEGIN { FS = "\t" }
{
    key = $1 "," $2
    glyph[key] = $5
    back[key] = $3
    reverse[key] = $4
    if ($2 > widest[$1]) widest[$1] = $2
    rows[$1] = 1
    if ($4 == 1) {
        reversed++
        if (reversed <= 6) printf "      row %d col %d is reverse video\n", $1, $2
    }
}
END {
    for (r in rows) {
        edges = 0
        for (c = 0; c <= widest[r]; c++) {
            g = glyph[r "," c]
            if (g == "\342\224\202" || g == "\342\224\214" || g == "\342\224\220" ||
                g == "\342\224\224" || g == "\342\224\230") edge[++edges] = c
        }
        for (k = 1; k + 1 <= edges; k += 2) {
            # a box whose side is covered by a box drawn over it leaves the
            # row with a corner where its other side should be, and pairing
            # across that would sweep the buffer between two boxes as if it
            # were an interior. Under-checking such a row is the safe way
            # to be wrong; reporting the buffer as a bleed is not.
            left = glyph[r "," edge[k]]
            right = glyph[r "," edge[k + 1]]
            if (left == "\342\224\220" || left == "\342\224\230") continue
            if (right == "\342\224\214" || right == "\342\224\224") continue
            for (c = edge[k]; c <= edge[k + 1]; c++) {
                key = r "," c
                cells++
                if (back[key] == "-") note(r, c, "the terminal default background")
                else if (back[key] in beneath) note(r, c, "the background of the layer beneath (" back[key] ")")
                else if (back[key] == float_bg) themed++
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

# The current screen, as a cell table and as the plain text derived from it.
# One capture serves both, so a wait that matched some text and an assertion
# about the colors under it are reading the same frame rather than two
# frames a poll apart.
capture() {
    tmux capture-pane -t "$SESSION" -p -e >"$CAP" 2>/dev/null || : >"$CAP"
    LC_ALL=C awk "$CELL_AWK" "$CAP" >"$CELLS"
    LC_ALL=C awk -F'\t' '
        { if (NR > 1 && $1 != prev) printf "\n"; printf "%s", $5; prev = $1 }
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
    return 1
}

pass() { printf 'ok   [%s] %s\n' "$CURRENT_LEG" "$1"; }

send_text() { tmux send-keys -t "$SESSION" -l -- "$1"; }
send_key() { tmux send-keys -t "$SESSION" "$1"; }
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

# Waits until at least one framed overlay is on screen, and its mirror.
wait_box() {
    local budget="$1" what="$2" start el
    start=$(now)
    while :; do
        capture
        grep -q '┌' "$SCREEN" && return 0
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what opened no framed overlay within ${budget}s"
            return 1
        fi
        sleep "$POLL"
    done
}
wait_no_box() {
    local budget="$1" what="$2" start el
    start=$(now)
    while :; do
        capture
        grep -q '┌' "$SCREEN" || return 0
        el=$(elapsed "$start" "$(now)")
        if ! under "$el" "$budget"; then
            fail "$what is still framed on screen after ${budget}s"
            return 1
        fi
        sleep "$POLL"
    done
}

assert_chrome() {
    local what="$1" findings
    findings=$(LC_ALL=C awk -v float_bg="$FLOAT_BG" \
        -v beneath_normal="$NORMAL_BG" -v beneath_cursor="$CURSORLINE_BG" \
        'BEGIN { beneath[beneath_normal] = 1; beneath[beneath_cursor] = 1 }'"$CHROME_AWK" \
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
    # anchored on the ruler rather than on any buffer text, which was
    # already on screen before the motion and so would report the cursor
    # moved whether it had or not
    local middle=$((ROWS / 2))
    send_text "${middle}G"
    wait_for "$middle,1" "$WAIT_SECS" "the cursor on the middle line" >/dev/null || return 1
}

end_session() {
    tmux kill-session -t "$SESSION" 2>/dev/null || true
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
ensure_artifact "$VIEW_BIN" "$REPO_ROOT/target/release/view" \
    cargo build --release -p view || exit 1
ensure_artifact "$STUB_BIN" "$REPO_ROOT/target/release/view-ai-stub-agent" \
    cargo build --release -p view-ai --features test-support --bin view-ai-stub-agent || exit 1
[ -d "$FIXTURE" ] || {
    printf 'FAIL: the themed fixture is missing at %s\n' "$FIXTURE" >&2
    exit 1
}

# A `#rrggbb` from the fixture colorscheme, as the decimal triple a
# truecolor escape spells it with. Read rather than repeated so a retuned
# fixture cannot leave the assertions matching a color nothing paints.
fixture_bg() {
    local group="$1" hex
    hex=$(grep -oE "'$group', \{[^}]*bg = '#[0-9a-f]{6}'" "$COLORSCHEME" |
        grep -oE "#[0-9a-f]{6}" | tail -1)
    if [ -z "$hex" ]; then
        printf 'FAIL: %s has no background in %s any more\n' "$group" "$COLORSCHEME" >&2
        return 1
    fi
    printf '%d;%d;%d' "0x${hex:1:2}" "0x${hex:3:2}" "0x${hex:5:2}"
}

NORMAL_BG=$(fixture_bg Normal) || exit 1
CURSORLINE_BG=$(fixture_bg CursorLine) || exit 1
FLOAT_BG=$(fixture_bg NormalFloat) || exit 1
for pair in "$NORMAL_BG:$CURSORLINE_BG" "$NORMAL_BG:$FLOAT_BG" "$CURSORLINE_BG:$FLOAT_BG"; do
    if [ "${pair%%:*}" = "${pair#*:}" ]; then
        printf 'FAIL: the fixture gives two of Normal/CursorLine/NormalFloat the same background (%s), so a bleed through an overlay is indistinguishable from correct paint\n' \
            "${pair%%:*}" >&2
        exit 1
    fi
done

FOCUSED_TITLE=$(grep -oE 'const FOCUSED_TITLE: &str = "[^"]+"' "$PANEL_RS" |
    sed -E 's/.*"(.*)"/\1/')
[ -n "$FOCUSED_TITLE" ] || {
    printf 'FAIL: FOCUSED_TITLE is not a &str constant in %s any more\n' "$PANEL_RS" >&2
    exit 1
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
    grep -oE '[0-9]+')
read_count=$(printf '%s\n' "$ENTRY_POINTS" | grep -c . || true)
if [ -z "$declared" ] || [ "$read_count" != "$declared" ]; then
    printf 'FAIL: %s declares %s default mappings and this read %s of them; the table has changed shape\n' \
        "$MAPPINGS_RS" "${declared:-no}" "$read_count" >&2
    exit 1
fi
# The features in registration order, deduplicated: the bare `:View
# <feature>` form each one answers is a separate entry point from its key,
# and the one that reaches a feature's canonical verb without naming it.
FEATURES=$(printf '%s\n' "$ENTRY_POINTS" | awk '!seen[$1]++ { print $1 }')

# nvim's own leader, which the fixture leaves at the default, as tmux must
# spell the key. A notation this cannot type fails loudly rather than being
# skipped: an entry point nothing drives is the hole this leg exists to
# close.
tmux_key() {
    local lhs="$1" typed="${1/<leader>/\\}"
    case "$typed" in
    *'<'* | *'>'*)
        printf 'FAIL: %s carries a key notation this script cannot type; teach tmux_key to spell it\n' "$lhs" >&2
        return 1
        ;;
    esac
    printf '%s' "$typed"
}

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
printf '\n[ai]\nagent = ["%s"]\n' "$STUB_BIN" >>"$CONFIG_HOME/view/view.toml"

CURRENT_LEG=theme-cache
SESSION="view-visual-$$-warm"
warm_root=$(mktemp -d "${TMPDIR:-/tmp}/view-visual-warm-XXXXXX")
ROOTS+=("$warm_root")
mkdir -p "$warm_root/xdg_data_home" "$warm_root/xdg_cache_home"
printf 'warm the theme cache\n' >"$warm_root/scratch.txt"
SESSIONS+=("$SESSION")
tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS" -c "$warm_root" \
    "env XDG_CONFIG_HOME=$CONFIG_HOME \
         XDG_DATA_HOME=$warm_root/xdg_data_home \
         XDG_STATE_HOME=$STATE_HOME \
         XDG_CACHE_HOME=$warm_root/xdg_cache_home \
         TERM=xterm-256color COLORTERM=truecolor \
         $VIEW_BIN $warm_root/scratch.txt"
wait_for 'warm the theme cache' "$WAIT_SECS" "the warming session's buffer" >/dev/null
# The float background as the cache stores it: the same value the
# assertions below look for on screen, proving the colorscheme reached
# view's own derivation and not merely nvim's.
FLOAT_BG_DECIMAL=$(awk -v triple="$FLOAT_BG" 'BEGIN {
    split(triple, c, ";")
    printf "%d", c[1] * 65536 + c[2] * 256 + c[3]
}')
warm_start=$(now)
while :; do
    if grep -A 2 -- '^\[chrome.NormalFloat\]' "$STATE_HOME"/view/theme-*.toml 2>/dev/null |
        grep -qE "^bg = $FLOAT_BG_DECIMAL\$"; then
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
    wait_for 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    assert_chrome 'the trust prompt'
    send_text 'y'
    wait_for "$FOCUSED_TITLE" "$WAIT_SECS" "the agent panel the answered trust prompt re-dispatches into" >/dev/null
    assert_chrome 'the agent panel'
    dismiss ai
    pass 'a feature named without its verb reaches the panel through the trust prompt'

    local feature lhs verb key took
    for feature in $FEATURES; do
        mark
        command_line ":View $feature"
        took=$(wait_change "$REACTION_SECS" ":View $feature")
        wait_box "$REACTION_SECS" ":View $feature" >/dev/null
        assert_chrome ":View $feature"
        dismiss "$feature"
        pass ":View $feature paints in ${took}s"
    done

    # fed from a here-document rather than a pipe: a pipeline's loop body
    # runs in a subshell, where a failed assertion would abort that subshell
    # and leave the session list this script cleans up behind incomplete
    while read -r feature lhs verb; do
        key=$(tmux_key "$lhs") || return 1
        mark
        send_text "$key"
        took=$(wait_change "$REACTION_SECS" "$lhs")
        wait_box "$REACTION_SECS" "$lhs" >/dev/null
        assert_chrome "$lhs ($feature $verb)"
        dismiss "$feature"
        pass "$lhs paints $feature $verb in ${took}s"
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
    wait_for 'Not an editor command' "$WAIT_SECS" "the error toast" >/dev/null
    assert_chrome 'the error toast'
    pass 'a mistyped command toasts over the cursor row, opaque'

    # and the history browser over it. An error toast is sticky (see
    # `toast::route`), so this is two overlays on screen at once, which is
    # the layered case the reported defect showed a box reading through in.
    mark
    command_line ':View notifications'
    wait_change "$REACTION_SECS" "the message history" >/dev/null
    wait_for 'Messages' "$WAIT_SECS" "the message history overlay" >/dev/null
    assert_chrome 'the message history over a standing toast'
    pass 'the message history opens over that toast, both opaque'

    end_session
}

leg_panel_typing() {
    CURRENT_LEG=panel-typing
    start_session typing 'visual sweep seed line'
    command_line ':View ai'
    wait_for 'Trust ' "$WAIT_SECS" "the project trust prompt" >/dev/null
    send_text 'y'
    wait_for "$FOCUSED_TITLE" "$WAIT_SECS" "the entered agent panel" >/dev/null

    # Three bursts rather than one: the reported defect was per keystroke
    # cost, and a composer that painted only its first burst would pass a
    # single one.
    local burst echoed typed tail_mark
    for burst in ALPHA BRAVO CHARLIE; do
        send_text "$burst"
        echoed=$(wait_for "$burst" "$REACTION_SECS" "the '$burst' keystroke burst in the composer")
        pass "the composer echoes '$burst' in ${echoed}s"
    done
    assert_chrome 'the agent panel mid-typing'

    # Past the composer's own width by a wide margin, whichever share of the
    # terminal the panel currently takes. The tail is what must still be on
    # screen: the cursor is at the end of the input, and a composer that
    # painted the head of it would be typing where the user cannot look.
    tail_mark=TAILMARK
    typed=$(awk 'BEGIN { for (i = 0; i < 24; i++) printf "wordwordword " }')
    send_text "$typed$tail_mark"
    echoed=$(wait_for "$tail_mark" "$WAIT_SECS" "the tail of a prompt longer than the panel is wide")
    assert_chrome 'the agent panel holding a wrapped prompt'
    pass "a $((${#typed} + ${#tail_mark}))-character prompt keeps its tail on screen (${echoed}s)"

    dismiss ai
    end_session
}

LEGS=(leg_entry_points leg_toast_and_history leg_panel_typing)
if [ "$#" -eq 0 ]; then
    selected=("${LEGS[@]}")
else
    selected=()
    for want in "$@"; do
        leg=${LEGS[$((want - 1))]:-}
        [ -n "$leg" ] || {
            printf 'FAIL: there is no leg %s (1..%s)\n' "$want" "${#LEGS[@]}" >&2
            exit 1
        }
        selected+=("$leg")
    done
fi
for leg in "${selected[@]}"; do "$leg"; done

printf 'visual sweep: %s of %s legs green\n' "${#selected[@]}" "${#LEGS[@]}"
