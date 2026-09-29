#!/usr/bin/env bash
#
# Cases for the first-frame contract in scripts/dogfood/lib.sh. record_gif
# keeps a recording hidden until the mark show_when_settled raises, a tape
# body opens with its own Show, and every tape's driver waits for the
# recorder and then for the settled editor before it types into or resizes
# the pane. A tape that broke any of the three recorded its own setup, or
# showed nothing for its whole length, and no check short of reading the
# gif frame by frame saw it.
#
# record_gif runs against stand-ins for vhs, tmux and fc-list, so no
# recording is made and no terminal is needed.
set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
LIB=$HERE/dogfood/lib.sh
TAPES=$HERE/dogfood/tapes
# shellcheck source=scripts/lib/scratch.sh
. "$HERE/lib/scratch.sh"
FAILED=0

WORK=$(mktemp -d "$(scratch_root)/record-gif-cases-XXXXXX")
cleanup_cases() {
    rm -rf -- "$WORK"
}
trap cleanup_cases EXIT

# shellcheck source=scripts/lib/case-report.sh
. "$HERE/lib/case-report.sh"

VHS_WANTED=$(sed -n 's/^RECORD_GIF_VHS_VERSION=//p' "$LIB")
mkdir -p "$WORK/bin" "$WORK/cache"
# the stand-ins report the versions record_gif requires unless a case
# names another
printf '#!/bin/sh\nif [ "$1" = --version ]; then echo "${VHS_STAND_IN:-vhs version %s}"; exit 0; fi\ncp -- "$1" "%s/tape.out"\n' \
    "$VHS_WANTED" "$WORK" >"$WORK/bin/vhs"
printf '#!/bin/sh\nif [ "$1" = -V ]; then echo "${TMUX_STAND_IN:-tmux 3.7c}"; fi\nexit 0\n' >"$WORK/bin/tmux"
printf '#!/bin/sh\necho stand-in\n' >"$WORK/bin/fc-list"
chmod +x "$WORK/bin/vhs" "$WORK/bin/tmux" "$WORK/bin/fc-list"

# Sources lib.sh in a subshell of its own, since the library arms an EXIT
# trap, and runs the words given against the stand-ins.
in_lib() {
    (
        PATH="$WORK/bin:$PATH"
        XDG_CACHE_HOME=$WORK/cache
        SOCKET=record-gif-cases
        # shellcheck source=scripts/dogfood/lib.sh
        . "$LIB"
        "$@"
    )
}

NL='
'

# the body contract: Show first, after Sleep lines at most
while IFS='|' read -r verdict body name; do
    body=$(printf '%b' "$body")
    if in_lib tape_body_opens_on_show "$body"; then
        got=accept
    else
        got=refuse
    fi
    if [ "$got" = "$verdict" ]; then
        report ok "body: $name"
    else
        report fail "body: $name" "expected $verdict, got $got"
    fi
done <<'CASES'
accept|Show|a bare Show
accept|Show\nSleep 3s|Show and its length
accept|Sleep 1s\nShow\nSleep 5s|a Sleep ahead of the Show
accept|\nShow\nSleep 2s|a blank line ahead of the Show
refuse|Sleep 2s|no Show at all
refuse|Type "x"\nShow|a key typed ahead of the Show
refuse|Sleep 1s\nHide\nShow|a Hide ahead of the Show
refuse|Wait /x/\nShow|a Wait of its own ahead of the Show
refuse|show\nSleep 1s|a Show spelled in the wrong case
CASES

# record_gif refuses a body that breaks the contract before vhs is run
rm -f "$WORK/tape.out"
set +e
in_lib record_gif record-gif-cases "$WORK/out.gif" 5 80 20 "Type \"x\"${NL}Show" \
    2>"$WORK/refused.err"
rc=$?
set -e
if [ "$rc" = 2 ] && [ ! -e "$WORK/tape.out" ] && grep -q 'opens with its own Show' "$WORK/refused.err"; then
    report ok "record_gif refuses a body that types ahead of its Show"
else
    report fail "record_gif refuses a body that types ahead of its Show" \
        "status $rc, vhs ran: $([ -e "$WORK/tape.out" ] && echo yes || echo no)"
fi

# the version build.sh stamps is the one record_gif requires, so a rebuilt
# recorder is never refused by the rig it was built for
if [ -n "$VHS_WANTED" ] &&
    grep -qx "VIEW_VHS_VERSION=$VHS_WANTED" "$HERE/dogfood/vhs/build.sh"; then
    report ok "build.sh stamps the vhs version record_gif requires ($VHS_WANTED)"
else
    report fail "build.sh stamps the vhs version record_gif requires" \
        "lib.sh requires '${VHS_WANTED}', build.sh stamps '$(sed -n 's/^VIEW_VHS_VERSION=//p' "$HERE/dogfood/vhs/build.sh")'"
fi

# record_gif runs only under the patched vhs and a tmux that applies the
# synchronized-output bracket, and a refusal names what it found
while IFS='|' read -r verdict tool reported; do
    rm -f "$WORK/tape.out"
    set +e
    if [ "$tool" = vhs ]; then
        VHS_STAND_IN=$reported in_lib record_gif record-gif-cases "$WORK/out.gif" 5 80 20 \
            2>"$WORK/version.err"
    else
        TMUX_STAND_IN=$reported in_lib record_gif record-gif-cases "$WORK/out.gif" 5 80 20 \
            2>"$WORK/version.err"
    fi
    rc=$?
    set -e
    if [ -e "$WORK/tape.out" ]; then ran=yes; else ran=no; fi
    name="$verdict $tool reporting \"$reported\""
    if [ "$verdict" = accept ] && [ "$rc" = 0 ] && [ "$ran" = yes ]; then
        report ok "$name"
    elif [ "$verdict" = refuse ] && [ "$rc" = 2 ] && [ "$ran" = no ] &&
        grep -qF "\"$reported\"" "$WORK/version.err" &&
        { [ "$tool" = tmux ] || grep -qF 'scripts/dogfood/vhs/build.sh' "$WORK/version.err"; }; then
        report ok "$name"
    else
        report fail "$name" "status $rc, vhs ran: $ran, said: $(cat "$WORK/version.err")"
    fi
done <<'CASES'
refuse|vhs|vhs version v0.11.0
refuse|vhs|vhs version v0.11.0-view10
refuse|vhs|vhs version unknown (built from source)
accept|vhs|vhs version v0.11.0-view1
refuse|tmux|tmux 3.6b
refuse|tmux|tmux 2.9a
refuse|tmux|tmux master
accept|tmux|tmux 3.7
accept|tmux|tmux 3.10
accept|tmux|tmux 4.0
accept|tmux|tmux next-3.8
CASES

# the tape record_gif hands vhs: hidden through the attach and the wait for
# the mark, shown only by the body
lib_value() {
    printf '%s' "${!1}"
}
ceiling=$(in_lib lib_value SETTLE_CEILING_TENTHS)
mark=$(in_lib lib_value SETTLED_MARK)
check_emitted() {
    local name=$1 body=$2 expected=$3
    rm -f "$WORK/tape.out"
    if [ -n "$body" ]; then
        in_lib record_gif record-gif-cases "$WORK/out.gif" 7 80 20 "$body" || true
    else
        in_lib record_gif record-gif-cases "$WORK/out.gif" 7 80 20 || true
    fi
    if [ ! -e "$WORK/tape.out" ]; then
        report fail "tape: $name" "vhs was never run"
        return
    fi
    local got
    got=$(sed -n '/^Hide$/,$p' "$WORK/tape.out")
    if [ "$got" = "$expected" ]; then
        report ok "tape: $name"
    else
        report fail "tape: $name" "vhs was handed${NL}$got${NL}where the case expects${NL}$expected"
    fi
}
prefix="Hide
Type \"tmux -L record-gif-cases attach -t cap\"
Enter
Wait+Screen@$((ceiling / 10 + 15))s /$mark/
Sleep 600ms"
check_emitted "no body shows at the mark for the tape's length" "" \
    "$prefix
Show
Sleep 7s"
check_emitted "a body plays from the mark" "Show${NL}Sleep 4s${NL}Hide${NL}Sleep 2s${NL}Show" \
    "$prefix
Show
Sleep 4s
Hide
Sleep 2s
Show"

# the recorder waits past the driver: a driver that gives up has said why
# before the recording fails
wait_s=$(sed -n 's/^Wait+Screen@\([0-9]*\)s .*/\1/p' "$WORK/tape.out")
if [ -n "$ceiling" ] && [ -n "$wait_s" ] && [ "$((wait_s * 10))" -gt "$ceiling" ]; then
    report ok "the recorder's ceiling sits past the driver's"
else
    report fail "the recorder's ceiling sits past the driver's" \
        "recorder ${wait_s:-?} s against the driver's ${ceiling:-?} tenths"
fi

# a still writes no gif, and takes its frame at the second mark once that
# mark has cleared
rm -f "$WORK/tape.out"
in_lib record_still record-gif-cases "$WORK/still/out.png" 80 20 || true
if [ ! -e "$WORK/tape.out" ]; then
    report fail "still: the tape record_still hands vhs" "vhs was never run"
else
    got=$(sed -n '/^Hide$/,$p' "$WORK/tape.out")
    expected="$prefix
Show
Wait+Screen@$((ceiling / 10 + 15))s /$mark/
Sleep 600ms
Screenshot \"$WORK/still/out.png\"
Sleep 100ms"
    if grep '^Output ' "$WORK/tape.out" >/dev/null; then
        report fail "still: the tape record_still hands vhs" "it names an Output: $(grep '^Output ' "$WORK/tape.out")"
    elif [ ! -d "$WORK/still" ]; then
        report fail "still: the tape record_still hands vhs" "the still's directory was never made"
    elif [ "$got" != "$expected" ]; then
        report fail "still: the tape record_still hands vhs" \
            "vhs was handed${NL}$got${NL}where the case expects${NL}$expected"
    else
        report ok "still: the tape record_still hands vhs"
    fi
fi

# Grades one tape script: a record_gif call that passes a body passes
# "$BODY", the body keeps the contract, and the driver waits for the
# recorder, then for the settled editor, then types or resizes. A
# record_still call passes no body, and its driver raises the mark a second
# time after its last key.
grade_tape() {
    local tape=$1 calls body wait_at settle_at verb_at settles last_settle last_verb
    calls=$(grep -cE '^record_(gif|still) ' "$tape" || true)
    if [ "$calls" = 0 ]; then
        echo "no record_gif or record_still call"
        return
    fi
    if grep -E '^record_(gif|still) ' "$tape" |
        grep -Ev '^record_gif "\$SOCKET" "\$OUT" [0-9]+ [0-9]+ [0-9]+( "\$BODY")?$|^record_still "\$SOCKET" "\$OUT" [0-9]+ [0-9]+$' >/dev/null; then
        echo "a record_gif call passes a body other than \"\$BODY\", or a record_still call passes one at all"
        return
    fi
    if grep -q '^BODY=' "$tape"; then
        body=$(awk -v q="'" '
            !on && index($0, "BODY=" q) == 1 { on = 1; $0 = substr($0, 7) }
            on && substr($0, length($0)) == q { print substr($0, 1, length($0) - 1); exit }
            on { print }' "$tape")
        if ! in_lib tape_body_opens_on_show "$body"; then
            echo "its BODY does not open with Show"
            return
        fi
    fi
    wait_at=$(grep -n 'wait_for_recorder "\$SOCKET"' "$tape" | head -1 | cut -d: -f1) || true
    settle_at=$(grep -n 'show_when_settled "\$SOCKET"' "$tape" | head -1 | cut -d: -f1) || true
    verb_at=$(grep -nE 'tmux -L "\$SOCKET" (send-keys|resize-window)' "$tape" | head -1 | cut -d: -f1) || true
    if [ -z "$wait_at" ] || [ -z "$settle_at" ]; then
        echo "its driver never waits for the recorder and the settled editor"
    elif [ "$settle_at" -lt "$wait_at" ]; then
        echo "its driver waits for the settled editor before the recorder"
    elif [ -n "$verb_at" ] && [ "$verb_at" -lt "$settle_at" ]; then
        echo "its driver types or resizes at line $verb_at, before the settled editor"
    elif grep '^record_still ' "$tape" >/dev/null; then
        # a still is taken at the second mark, so a driver that raises one
        # mark, or raises its second ahead of its last key, leaves the
        # recorder waiting or the frame taken on a half-built scene
        settles=$(grep -c 'show_when_settled "\$SOCKET"' "$tape" || true)
        last_settle=$(grep -n 'show_when_settled "\$SOCKET"' "$tape" | tail -1 | cut -d: -f1) || true
        last_verb=$(grep -nE 'tmux -L "\$SOCKET" (send-keys|resize-window)' "$tape" | tail -1 | cut -d: -f1) || true
        if [ "$settles" -lt 2 ]; then
            echo "its driver raises the settled mark once, and the still waits for a second"
        elif [ -n "$last_verb" ] && [ "$last_settle" -lt "$last_verb" ]; then
            echo "its driver types or resizes at line $last_verb, after the mark the still is taken at"
        fi
    fi
}

# warm_cargo_target runs a niced workspace check and stops the capture when
# the tree does not check
printf '#!/bin/sh\necho "cargo $*" >>"%s/cargo.log"\necho "error: stand-in" >&2\nexit "${CARGO_STAND_IN_STATUS:-0}"\n' \
    "$WORK" >"$WORK/bin/cargo"
printf '#!/bin/sh\necho "nice $*" >>"%s/cargo.log"\nshift 2\nexec "$@"\n' \
    "$WORK" >"$WORK/bin/nice"
chmod +x "$WORK/bin/cargo" "$WORK/bin/nice"
rm -f "$WORK/cargo.log"
set +e
in_lib warm_cargo_target "$WORK" cases 2>"$WORK/warm.err"
rc=$?
set -e
if [ "$rc" = 0 ] && grep -qx 'nice -n 15 cargo check --workspace --all-targets' "$WORK/cargo.log" &&
    grep -qx 'cargo check --workspace --all-targets' "$WORK/cargo.log" &&
    grep -q "cases: warming the cargo target dir $WORK/cache/view-dogfood-tapes/target" "$WORK/warm.err"; then
    report ok "warm_cargo_target checks the workspace niced and says so first"
else
    report fail "warm_cargo_target checks the workspace niced and says so first" \
        "status $rc, cargo ran: $(cat "$WORK/cargo.log" 2>/dev/null), said: $(cat "$WORK/warm.err")"
fi
set +e
CARGO_STAND_IN_STATUS=101 in_lib warm_cargo_target "$WORK" cases 2>"$WORK/warm.err"
rc=$?
set -e
if [ "$rc" = 2 ] && grep -q 'cases: cargo check failed' "$WORK/warm.err" &&
    grep -qx 'error: stand-in' "$WORK/warm.err"; then
    report ok "warm_cargo_target stops the capture and shows why when the tree does not check"
else
    report fail "warm_cargo_target stops the capture and shows why when the tree does not check" \
        "status $rc, said: $(cat "$WORK/warm.err")"
fi

# a tape that opens a Rust file warms the target dir before its session
# starts, and so does cap.sh when the file it opens is one
for script in "$TAPES/tiled-panes.sh" "$TAPES/hero.sh" "$HERE/dogfood/cap.sh"; do
    warm_at=$(grep -n 'warm_cargo_target "\$ROOT"' "$script" | head -1 | cut -d: -f1) || true
    session_at=$(grep -n '^new_cap_session ' "$script" | head -1 | cut -d: -f1) || true
    if [ -n "$warm_at" ] && [ -n "$session_at" ] && [ "$warm_at" -lt "$session_at" ]; then
        report ok "$(basename -- "$script") warms the target dir before its session"
    else
        report fail "$(basename -- "$script") warms the target dir before its session" \
            "warm_cargo_target at line ${warm_at:-none}, new_cap_session at line ${session_at:-none}"
    fi
done

# cap.sh warms the target dir for a Rust file alone, run against the same
# stand-ins, so no session starts and no capture is made
while IFS='|' read -r opens warms; do
    rm -f "$WORK/cargo.log"
    set +e
    PATH="$WORK/bin:$PATH" XDG_CACHE_HOME=$WORK/cache VIEW_BIN=$WORK/bin/tmux \
        sh "$HERE/dogfood/cap.sh" --settle 0 "$WORK/cap/out.txt" -- "$opens" \
        2>"$WORK/cap.err"
    rc=$?
    set -e
    if [ -e "$WORK/cargo.log" ]; then warmed=yes; else warmed=no; fi
    if [ "$rc" = 0 ] && [ "$warmed" = "$warms" ]; then
        report ok "a cap.sh capture of $opens warms the target dir: $warms"
    else
        report fail "a cap.sh capture of $opens warms the target dir: $warms" \
            "status $rc, warmed: $warmed, said: $(cat "$WORK/cap.err")"
    fi
done <<'EOF'
crates/view-core/src/model/look.rs|yes
README.md|no
EOF

shipped=0
for tape in "$TAPES"/*.sh; do
    [ -e "$tape" ] || continue
    shipped=$((shipped + 1))
    finding=$(grade_tape "$tape")
    if [ -z "$finding" ]; then
        report ok "shipped: $(basename -- "$tape")"
    else
        report fail "shipped: $(basename -- "$tape")" "$finding"
    fi
done
if [ "$shipped" -lt 4 ]; then
    report fail "shipped tapes" "the walk read $shipped tape scripts under $TAPES"
fi

# planted tapes, each breaking one rule the walk grades
plant() {
    printf '%s\n' "$2" >"$WORK/$1.sh"
}
plant keys-first '(
  wait_for_recorder "$SOCKET"
  tmux -L "$SOCKET" send-keys -t cap x
  show_when_settled "$SOCKET"
) &
record_gif "$SOCKET" "$OUT" 5 220 50'
plant no-settle '(
  wait_for_recorder "$SOCKET"
  tmux -L "$SOCKET" send-keys -t cap x
) &
record_gif "$SOCKET" "$OUT" 5 220 50'
plant settle-before-attach '(
  show_when_settled "$SOCKET"
  wait_for_recorder "$SOCKET"
) &
record_gif "$SOCKET" "$OUT" 5 220 50'
plant hidden-body "(
  wait_for_recorder \"\$SOCKET\"
  show_when_settled \"\$SOCKET\"
) &
BODY='Sleep 3s
Type \"x\"
Show'
record_gif \"\$SOCKET\" \"\$OUT\" 5 220 50 \"\$BODY\""
plant inline-body "(
  wait_for_recorder \"\$SOCKET\"
  show_when_settled \"\$SOCKET\"
) &
record_gif \"\$SOCKET\" \"\$OUT\" 5 220 50 'Type x'"
plant still-one-mark '(
  wait_for_recorder "$SOCKET"
  show_when_settled "$SOCKET"
) &
record_still "$SOCKET" "$OUT" 220 50'
plant still-mark-before-keys '(
  wait_for_recorder "$SOCKET"
  show_when_settled "$SOCKET"
  show_when_settled "$SOCKET" x
  tmux -L "$SOCKET" send-keys -t cap x
) &
record_still "$SOCKET" "$OUT" 220 50'
plant still-body "(
  wait_for_recorder \"\$SOCKET\"
  show_when_settled \"\$SOCKET\"
  show_when_settled \"\$SOCKET\" x
) &
BODY='Show'
record_still \"\$SOCKET\" \"\$OUT\" 220 50 \"\$BODY\""
for planted in keys-first no-settle settle-before-attach hidden-body inline-body \
    still-one-mark still-mark-before-keys still-body; do
    finding=$(grade_tape "$WORK/$planted.sh")
    if [ -n "$finding" ]; then
        report ok "planted $planted is refused: $finding"
    else
        report fail "planted $planted" "the walk passed it"
    fi
done

exit "$FAILED"
